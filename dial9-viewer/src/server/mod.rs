use crate::storage::{LocalBackend, StorageBackend};
use axum::Router;
use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::cors::CorsLayer;
use tower_http::timeout::TimeoutLayer;

mod browse;
mod buckets;
#[cfg(feature = "s3")]
mod check;
mod config;
#[cfg(feature = "s3")]
pub mod credentials;
#[cfg(not(feature = "s3"))]
#[path = "credentials_disabled.rs"]
pub mod credentials;
mod error;
pub(crate) mod flamegraph;
pub(crate) mod fold_stream;
pub(crate) mod metrics;
mod prefixes;
#[cfg(feature = "s3")]
mod s3;
#[cfg(not(feature = "s3"))]
#[path = "s3_disabled.rs"]
mod s3;
mod services;
pub(crate) mod span_stats;
pub(crate) mod tokio_stats;
mod trace;
mod upload;

pub use upload::{UploadLimits, UploadStore};

use credentials::MaybeCreds;

#[cfg(feature = "s3")]
pub(crate) use s3::region_from_head_bucket;

// Embed ONLY the built artifact set (`npm run build` output), never sources,
// tests, or node_modules. A cargo-only checkout still compiles: the committed
// `ui/dist/.gitkeep` keeps the folder present (empty UI until built). Release
// CI runs the JS build before packaging, so published crates/binaries carry a
// populated dist. See docs/adr/0004-viewer-ui-migration.md section 3.
#[derive(Embed)]
#[folder = "ui/dist/"]
struct UiAssets;

pub(crate) fn embedded_ui_asset(path: &str) -> Option<Vec<u8>> {
    UiAssets::get(path).map(|file| file.data.into_owned())
}

/// Default output key prefix for aggregate part-files.
const DEFAULT_AGG_OUTPUT_PREFIX: &str = "flamegraph-data";
const RAW_SPAN_STATS_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Destination for the aggregate part-files produced by demand-driven folding.
///
/// This is always a **server-owned** backend — never the request's source
/// backend or the caller's bring-your-own credentials. That is the invariant
/// that lets aggregation run against a read-only source bucket without failing
/// on the first `PutObject`, and guarantees a caller's keys are never used for
/// writes. There are two shapes:
///   - [`AggOutput::s3`] persists parts to an operator-owned S3 bucket, so
///     rollups survive restarts. Built from `--agg-output-bucket`.
///   - [`AggOutput::temporary`] writes to a fresh process-local temporary
///     directory that is removed at shutdown; rollups are recomputed after a
///     restart. This is the default when no output bucket is configured.
///
/// The backend, bucket, and prefix always travel together, so they are one
/// value rather than three independently-optional fields on [`AppState`].
#[derive(Clone)]
pub struct AggOutput {
    /// Backend all aggregate writes go through.
    backend: Arc<dyn StorageBackend>,
    /// The S3 output bucket, or `None` for the process-local temporary
    /// directory (whose [`LocalBackend`] ignores the bucket argument, so the
    /// per-request source bucket rides along as an inert placeholder).
    bucket: Option<String>,
    /// Output key prefix (default [`DEFAULT_AGG_OUTPUT_PREFIX`]).
    prefix: String,
    /// Human-readable destination, captured at construction for startup logging
    /// (the temp path is only reachable on the concrete backend, not `dyn`).
    location: String,
}

impl AggOutput {
    /// Persist aggregate parts to the operator-owned S3 `bucket` via `backend`
    /// (built once at startup with the server's ambient identity, region-aware).
    pub fn s3(bucket: impl Into<String>, backend: Arc<dyn StorageBackend>) -> Self {
        let bucket = bucket.into();
        let location = format!("s3://{bucket}");
        Self {
            backend,
            bucket: Some(bucket),
            prefix: DEFAULT_AGG_OUTPUT_PREFIX.to_string(),
            location,
        }
    }

    /// Write aggregate parts to a fresh process-local temporary directory,
    /// removed when this value's last clone drops at server shutdown.
    pub fn temporary() -> Self {
        let backend = LocalBackend::new_temporary_aggregate();
        let location = format!("temporary local directory ({})", backend.root().display());
        Self {
            backend: Arc::new(backend),
            bucket: None,
            prefix: DEFAULT_AGG_OUTPUT_PREFIX.to_string(),
            location,
        }
    }

    /// Override the output key prefix (default `flamegraph-data`).
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    pub(crate) fn backend(&self) -> Arc<dyn StorageBackend> {
        Arc::clone(&self.backend)
    }

    pub(crate) fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The bucket argument for writes: the configured S3 output bucket, or the
    /// request's `source_bucket` as an inert placeholder for the local
    /// temporary backend (which ignores it).
    pub(crate) fn output_bucket_for(&self, source_bucket: &str) -> String {
        self.bucket
            .clone()
            .unwrap_or_else(|| source_bucket.to_string())
    }

    /// Human-readable destination, for startup logging.
    pub(crate) fn location(&self) -> &str {
        &self.location
    }
}

#[derive(Clone)]
#[non_exhaustive]
pub struct AppState {
    pub backend: Arc<dyn StorageBackend>,
    pub default_bucket: Option<String>,
    pub default_prefix: Option<String>,
    /// When set, serve UI files from disk instead of embedded assets.
    pub dev_ui_dir: Option<PathBuf>,
    /// When set, `/api/flamegraph` runs the demand-driven refinement loop
    /// against these backends instead of reading a pre-aggregated local dir.
    pub agg: Option<crate::ingest::aggregate::AggContext>,
    /// In-memory store of temporary, POSTed traces. `None` (the default) means
    /// the trace-upload feature is disabled and its routes are not registered.
    pub uploads: Option<Arc<UploadStore>>,
    /// Whether the UI should offer the bring-your-own-credentials panel and
    /// whether handlers honor `x-dial9-aws-*` headers. True for S3 backends,
    /// false for `--local-dir`. This same flag also gates on-demand
    /// aggregation: any S3 bucket can run the `/api/flamegraph` refinement loop,
    /// but a local-directory source cannot.
    pub allow_byo_creds: bool,
    /// Whether source keys use the production date-partitioned S3 layout.
    ///
    /// This controls time-scoped browse and service-discovery listings. It is
    /// independent of credentials because non-S3 backends such as the simulator
    /// can expose the same layout.
    time_partitioned_source: bool,
    /// Optional plumbing for ephemeral S3 client construction (test injection
    /// of the in-process fake; `None` in production → default HTTPS connector).
    #[doc(hidden)]
    #[cfg(feature = "s3")]
    pub ephemeral_s3: Option<crate::storage::EphemeralS3Config>,
    /// Mints credentials for the assume-role path (`x-dial9-aws-role-arn`). When
    /// `None`, role-arn requests are refused (the server has no identity wired to
    /// do the assuming); production sets the STS-backed assumer, tests inject a
    /// fake. Independent of `allow_byo_creds` so a deployment can offer one path,
    /// both, or neither.
    #[cfg(feature = "s3")]
    pub role_assumer: Option<Arc<dyn credentials::RoleAssumer>>,
    /// Destination for BYOC aggregate part-files: an operator-owned S3 bucket
    /// (persistent) or a process-local temporary directory (the default,
    /// removed at shutdown). Always a server-owned backend — aggregate writes
    /// never use the request's source credentials. See [`AggOutput`].
    pub agg_output: AggOutput,
    /// Segment duration (seconds) for BYOC aggregation scope padding.
    pub agg_segment_secs: i64,
    /// Bucket-name substring the UI's bucket picker filters on, advertised via
    /// `/api/config` as `bucket_filter` (the filtering itself is client-side).
    /// Defaults to "dial9"; empty disables filtering. See
    /// [`Self::with_bucket_filter`].
    pub bucket_filter: String,
    /// Process-global concurrency limits for the demand-driven fold pipeline,
    /// shared across all in-flight `/api/flamegraph` requests so total fold work
    /// is bounded application-wide (see [`FoldLimits`]).
    pub(crate) fold_limits: crate::ingest::aggregate::FoldLimits,
}

impl AppState {
    pub fn new(
        backend: Arc<dyn StorageBackend>,
        default_bucket: Option<String>,
        default_prefix: Option<String>,
    ) -> Self {
        Self {
            backend,
            default_bucket,
            default_prefix,
            dev_ui_dir: None,
            agg: None,
            uploads: None,
            allow_byo_creds: false,
            time_partitioned_source: false,
            #[cfg(feature = "s3")]
            ephemeral_s3: None,
            #[cfg(feature = "s3")]
            role_assumer: None,
            agg_output: AggOutput::temporary(),
            agg_segment_secs: crate::ingest::aggregate::DEFAULT_SEGMENT_DURATION_SECS,
            bucket_filter: "dial9".to_string(),
            fold_limits: crate::ingest::aggregate::FoldLimits::default(),
        }
    }

    /// Build an `AppState` backed by a local directory.
    ///
    /// ```ignore
    /// let state = AppState::from_local_dir("/tmp/my-traces");
    /// let app = dial9_viewer::server::router(state);
    /// ```
    pub fn from_local_dir(dir: impl AsRef<std::path::Path>) -> Self {
        let backend = Arc::new(crate::storage::LocalBackend::new(dir.as_ref()));
        Self::new(backend, Some("local".into()), None)
    }

    pub fn with_dev_ui_dir(mut self, dir: PathBuf) -> Self {
        self.dev_ui_dir = Some(dir);
        self
    }

    pub fn with_agg(mut self, agg: crate::ingest::aggregate::AggContext) -> Self {
        self.agg = Some(agg);
        self
    }

    /// Enable the temporary trace-upload feature with the given caps. Without
    /// this, `POST /api/upload` and `GET /api/uploaded/{id}` are not registered.
    pub fn with_uploads(mut self, limits: UploadLimits) -> Self {
        self.uploads = Some(Arc::new(UploadStore::new(limits)));
        self
    }

    /// Enable the bring-your-own-credentials path (S3 backends only).
    ///
    /// Enabling it also selects the production time-partitioned source layout
    /// for backwards compatibility with existing S3 embedders.
    pub fn with_byo_creds(mut self, allow: bool) -> Self {
        self.allow_byo_creds = allow;
        if allow {
            self.time_partitioned_source = true;
        }
        self
    }

    /// Use production date/time-partitioned trace keys for browse and service
    /// discovery without enabling AWS credential handling.
    pub fn with_time_partitioned_source(mut self) -> Self {
        self.time_partitioned_source = true;
        self
    }

    /// Set the destination for BYOC aggregate part-files. Defaults to a
    /// process-local temporary directory ([`AggOutput::temporary`]); pass
    /// [`AggOutput::s3`] to persist to an operator-owned bucket. In every case
    /// the source backend is never used for aggregate writes.
    pub fn with_agg_output(mut self, output: AggOutput) -> Self {
        self.agg_output = output;
        self
    }

    pub fn with_agg_segment_secs(mut self, secs: i64) -> Self {
        self.agg_segment_secs = secs;
        self
    }

    /// Set the bucket-name substring the UI's bucket picker uses to surface
    /// trace buckets, advertised to clients via `/api/config` as
    /// `bucket_filter` (T15; the match is case-insensitive and happens
    /// client-side). Defaults to "dial9"; pass an empty string to disable the
    /// filtering. A page can still override the advertised value per load with
    /// a `bucket_filter=` query param on its own URL.
    pub fn with_bucket_filter(mut self, filter: impl Into<String>) -> Self {
        self.bucket_filter = filter.into();
        self
    }

    /// Build a per-request [`AggContext`] for the bring-your-own-credentials
    /// path (`?bucket=…`), or reuse the server-configured one.
    ///
    /// When `bucket` is `Some`, the request targets the user's own bucket:
    /// resolve a backend from any supplied credentials, scope the source listing
    /// to `prefix` (falling back to the server default), and route output to the
    /// configured `--agg-output-bucket` or the shared process-local temporary
    /// directory. The request's source backend is never used for aggregate
    /// writes. When `bucket` is `None`, fall back to the server's `--agg`
    /// context if one is configured.
    ///
    /// Returns `None` only when no `bucket` is given *and* the server has no
    /// `--agg` context — the caller maps that to 404. This is the single place
    /// the BYOC context is assembled, shared by `/api/flamegraph` and
    /// `/tokio-stats`.
    ///
    /// [`AggContext`]: crate::ingest::aggregate::AggContext
    pub(crate) async fn agg_context_for(
        &self,
        bucket: Option<&str>,
        prefix: Option<&str>,
        aws_region: Option<&str>,
        creds: MaybeCreds,
    ) -> Result<Option<crate::ingest::aggregate::AggContext>, (StatusCode, String)> {
        use crate::ingest::aggregate::AggContext;
        if let Some(bucket) = bucket {
            let backend = self.resolve_with_region(creds, aws_region).await?;
            let source_prefix = prefix
                .map(str::to_string)
                .or_else(|| self.default_prefix.clone())
                .unwrap_or_default();
            // Output goes through the server-owned backend, never through the
            // request's source credentials: either the configured S3 output
            // bucket or the process-local temporary directory.
            let output_bucket = self.agg_output.output_bucket_for(bucket);
            tracing::info!(
                %bucket,
                %output_bucket,
                resolved_source_prefix = %source_prefix,
                output_prefix = %self.agg_output.prefix(),
                "agg: BYOC context"
            );
            Ok(Some(AggContext {
                source: backend,
                output: self.agg_output.backend(),
                source_bucket: bucket.to_string(),
                source_is_local: false,
                output_bucket,
                output_prefix: self.agg_output.prefix().to_string(),
                source_prefixes: vec![source_prefix],
                segment_duration_secs: self.agg_segment_secs,
            }))
        } else {
            Ok(self.agg.clone())
        }
    }
}

pub fn router(state: AppState) -> Router {
    if let Some(dir) = state.dev_ui_dir.clone() {
        tracing::info!(path = %dir.display(), "serving UI from disk (dev mode)");
        Router::new()
            .nest("/api", api_router(state))
            .fallback_service(tower_http::services::ServeDir::new(dir))
    } else {
        Router::new()
            .nest("/api", api_router(state))
            .fallback(serve_embedded)
    }
}

async fn serve_embedded(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match UiAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_str(mime.as_ref()).unwrap(),
                )
                .body(Body::from(file.data.to_vec()))
                .unwrap()
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn api_router(state: AppState) -> Router {
    // Body limit for the upload route. When uploads are disabled there's no
    // configured store, so fall back to the default cap — the handler rejects
    // the request with 404 anyway, this just bounds buffering until it does.
    let upload_body_limit = state
        .uploads
        .as_ref()
        .map(|u| u.max_upload_bytes())
        .unwrap_or_else(|| UploadLimits::default().max_upload_bytes());

    // The upload route gets its own (large) body limit; other routes keep
    // axum's conservative default. The trace-upload feature is opt-in
    // (`dial9 serve --enable-upload`): the routes are always present, but when
    // uploads are disabled the handlers return 404, as if the feature were
    // absent. (Registering unconditionally keeps the status deterministic
    // regardless of the static-file fallback, which 405s POSTs in dev mode.)
    let upload_route = Router::new()
        .route("/upload", axum::routing::post(upload::upload_trace))
        .layer(DefaultBodyLimit::max(upload_body_limit));
    let raw_span_stats_post = axum::routing::post(span_stats::post_raw_span_stats)
        .route_layer(DefaultBodyLimit::max(span_stats::RAW_SPAN_STATS_BODY_LIMIT))
        // Acquire capacity before Bytes extraction buffers the request body.
        .route_layer(raw_span_stats_concurrency_limit())
        // Bound body buffering and decode work. This is outermost so timing out
        // drops the inner future and releases its concurrency permit.
        .route_layer(raw_span_stats_timeout(RAW_SPAN_STATS_REQUEST_TIMEOUT));
    let span_stats_route = Router::new().route(
        "/span-stats",
        axum::routing::get(span_stats::get_span_stats).merge(raw_span_stats_post),
    );

    let router = Router::new()
        .route("/config", axum::routing::get(config::get_config))
        .route("/buckets", axum::routing::get(buckets::list_buckets))
        .route("/prefixes", axum::routing::get(prefixes::list_prefixes))
        .route("/services", axum::routing::get(services::list_services))
        .route("/browse", axum::routing::get(browse::browse))
        .route("/object", axum::routing::get(trace::get_object))
        .route(
            "/flamegraph",
            axum::routing::get(flamegraph::get_flamegraph),
        )
        .route(
            "/tokio-stats",
            axum::routing::get(tokio_stats::get_tokio_stats),
        )
        .route("/uploaded/{id}", axum::routing::get(upload::get_uploaded));
    let router = s3::add_routes(router);
    router
        .merge(upload_route)
        .merge(span_stats_route)
        // Permissive CORS so a page on another origin can POST a trace and read
        // it back via fetch(); also answers the OPTIONS preflight automatically.
        .layer(CorsLayer::permissive())
        // Per-request metrics. Layered on the API router (not the outer one) so
        // it sees the populated `MatchedPath` and only counts API requests, not
        // static-asset fetches. Publishes to the global `ServiceMetrics` sink.
        .layer(axum::middleware::from_fn(metrics::record_request_metrics))
        .with_state(state)
}

fn raw_span_stats_concurrency_limit() -> ConcurrencyLimitLayer {
    ConcurrencyLimitLayer::new(span_stats::MAX_CONCURRENT_RAW_SPAN_STATS)
}

fn raw_span_stats_timeout(timeout: Duration) -> TimeoutLayer {
    TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::http::Request;
    use futures::stream;
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;
    use tower::{Layer, ServiceExt, service_fn};

    /// The temporary output uses the request's source bucket as the (ignored)
    /// bucket argument, while an S3 output always writes to its own bucket
    /// regardless of the source. This is the routing that keeps aggregate
    /// writes off the caller's (possibly read-only) source bucket.
    #[test]
    fn output_bucket_for_routes_temp_to_source_and_s3_to_configured() {
        let temp = AggOutput::temporary();
        assert_eq!(temp.output_bucket_for("caller-source"), "caller-source");

        let backend: Arc<dyn StorageBackend> = Arc::new(LocalBackend::new_temporary_aggregate());
        let s3 = AggOutput::s3("operator-owned", backend);
        assert_eq!(s3.output_bucket_for("caller-source"), "operator-owned");
    }

    #[tokio::test]
    async fn raw_span_stats_concurrency_is_bounded() {
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let service = raw_span_stats_concurrency_limit().layer(service_fn({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |()| {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    release.acquire().await.unwrap().forget();
                    Ok::<_, Infallible>(())
                }
            }
        }));

        let mut requests = Vec::new();
        for _ in 0..span_stats::MAX_CONCURRENT_RAW_SPAN_STATS {
            requests.push(tokio::spawn(service.clone().oneshot(())));
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) < span_stats::MAX_CONCURRENT_RAW_SPAN_STATS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("configured number of requests should start");

        requests.push(tokio::spawn(service.clone().oneshot(())));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            started.load(Ordering::SeqCst),
            span_stats::MAX_CONCURRENT_RAW_SPAN_STATS,
            "an additional request must wait for decode capacity"
        );

        release.add_permits(span_stats::MAX_CONCURRENT_RAW_SPAN_STATS);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) <= span_stats::MAX_CONCURRENT_RAW_SPAN_STATS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiting request should start after capacity is released");
        release.add_permits(1);
        for request in requests {
            request.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn raw_span_stats_stalled_bodies_time_out_and_release_capacity() {
        let route = axum::routing::post(|_: Bytes| async { StatusCode::OK })
            .route_layer(DefaultBodyLimit::max(1024))
            .route_layer(raw_span_stats_concurrency_limit())
            .route_layer(raw_span_stats_timeout(Duration::from_millis(50)));
        let app = Router::new().route("/", route);
        let stalled_request = || {
            Request::post("/")
                .body(Body::from_stream(stream::pending::<
                    Result<Bytes, Infallible>,
                >()))
                .unwrap()
        };

        let first = tokio::spawn(app.clone().oneshot(stalled_request()));
        let second = tokio::spawn(app.clone().oneshot(stalled_request()));
        tokio::time::sleep(Duration::from_millis(10)).await;
        let waiting = tokio::spawn(
            app.clone()
                .oneshot(Request::post("/").body(Body::from("complete")).unwrap()),
        );

        assert_eq!(
            first.await.unwrap().unwrap().status(),
            StatusCode::REQUEST_TIMEOUT
        );
        assert_eq!(
            second.await.unwrap().unwrap().status(),
            StatusCode::REQUEST_TIMEOUT
        );
        assert_eq!(waiting.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    /// The default output prefix is applied and overridable, and the default
    /// matches the CLI's `--agg-output-prefix` default.
    #[test]
    fn agg_output_prefix_defaults_and_overrides() {
        assert_eq!(AggOutput::temporary().prefix(), DEFAULT_AGG_OUTPUT_PREFIX);
        assert_eq!(
            AggOutput::temporary().with_prefix("custom").prefix(),
            "custom"
        );
    }
}
