use crate::storage::{EphemeralS3Config, LocalBackend, S3Backend, StorageBackend};
use axum::Router;
use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

mod browse;
mod buckets;
mod check;
mod config;
pub mod credentials;
mod error;
pub(crate) mod flamegraph;
pub(crate) mod metrics;
mod prefixes;
pub(crate) mod tokio_stats;
mod trace;
mod upload;

pub use upload::{UploadLimits, UploadStore};

use credentials::{CredError, CredSource, MaybeCreds};

/// Detect a bucket's region via `HeadBucket`, reading `bucket_region()` on
/// success and the `x-amz-bucket-region` response header on the redirect error
/// that S3 returns when the client's region doesn't match the bucket's.
///
/// Shared by startup region detection ([`crate::build_app`]) and the
/// `/api/credentials/check` endpoint.
pub(crate) async fn region_from_head_bucket(
    client: &aws_sdk_s3::Client,
    bucket: &str,
) -> Option<String> {
    match client.head_bucket().bucket(bucket).send().await {
        Ok(resp) => resp.bucket_region().map(|r| r.to_string()),
        Err(err) => err.raw_response().and_then(|r| {
            r.headers()
                .get("x-amz-bucket-region")
                .map(|v| v.to_string())
        }),
    }
}

#[derive(Embed)]
#[folder = "ui/"]
struct UiAssets;

/// Default output key prefix for aggregate part-files.
const DEFAULT_AGG_OUTPUT_PREFIX: &str = "flamegraph-data";

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
    /// Optional plumbing for ephemeral S3 client construction (test injection
    /// of the in-process fake; `None` in production → default HTTPS connector).
    #[doc(hidden)]
    pub ephemeral_s3: Option<EphemeralS3Config>,
    /// Mints credentials for the assume-role path (`x-dial9-aws-role-arn`). When
    /// `None`, role-arn requests are refused (the server has no identity wired to
    /// do the assuming); production sets the STS-backed assumer, tests inject a
    /// fake. Independent of `allow_byo_creds` so a deployment can offer one path,
    /// both, or neither.
    pub role_assumer: Option<Arc<dyn credentials::RoleAssumer>>,
    /// Destination for BYOC aggregate part-files: an operator-owned S3 bucket
    /// (persistent) or a process-local temporary directory (the default,
    /// removed at shutdown). Always a server-owned backend — aggregate writes
    /// never use the request's source credentials. See [`AggOutput`].
    pub agg_output: AggOutput,
    /// Segment duration (seconds) for BYOC aggregation scope padding.
    pub agg_segment_secs: i64,
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
            ephemeral_s3: None,
            role_assumer: None,
            agg_output: AggOutput::temporary(),
            agg_segment_secs: crate::ingest::aggregate::DEFAULT_SEGMENT_DURATION_SECS,
            fold_limits: crate::ingest::aggregate::FoldLimits::default(),
        }
    }

    /// Build an `AppState` backed by an S3 bucket, with automatic region
    /// detection, bring-your-own-credentials support, and the assume-role
    /// credential path enabled.
    ///
    /// This is the high-level entry point for embedders who want to serve
    /// traces from S3 without replicating the CLI's setup logic:
    ///
    /// ```ignore
    /// let state = AppState::from_bucket("my-traces", None).await;
    /// let app = dial9_viewer::server::router(state);
    /// // … customize app, then bind …
    /// ```
    pub async fn from_bucket(bucket: impl Into<String>, prefix: Option<String>) -> Self {
        let bucket = bucket.into();
        let backend = Arc::new(crate::s3_backend_for(&bucket).await);
        let assumer = credentials::StsRoleAssumer::from_env().await;
        Self::new(backend, Some(bucket), prefix)
            .with_byo_creds(true)
            .with_role_assumer(Arc::new(assumer))
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

    /// Enable the bring-your-own-credentials path (S3 backends only). This also
    /// enables on-demand aggregation; leave unset (the default) for local-dir
    /// sources, where credentials are meaningless and aggregation is local.
    pub fn with_byo_creds(mut self, allow: bool) -> Self {
        self.allow_byo_creds = allow;
        self
    }

    /// Inject ephemeral-S3 plumbing (test seam; production leaves this unset).
    #[doc(hidden)]
    pub fn with_ephemeral_s3(mut self, cfg: EphemeralS3Config) -> Self {
        self.ephemeral_s3 = Some(cfg);
        self
    }

    /// Enable the assume-role credential path with the given assumer. Production
    /// passes an [`crate::server::credentials::StsRoleAssumer`]; tests inject a
    /// fake. Without this, `x-dial9-aws-role-arn` requests are refused.
    pub fn with_role_assumer(mut self, assumer: Arc<dyn credentials::RoleAssumer>) -> Self {
        self.role_assumer = Some(assumer);
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
        creds: MaybeCreds,
    ) -> Result<Option<crate::ingest::aggregate::AggContext>, (StatusCode, String)> {
        use crate::ingest::aggregate::AggContext;
        if let Some(bucket) = bucket {
            let backend = self.resolve(creds).await?;
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

    /// Pick the storage backend for a request given its credential source.
    ///
    /// Supplying credentials is always optional, and the two credentialed
    /// transports are alternatives (the extractor already rejected supplying
    /// both):
    /// - malformed/incomplete/conflicting headers → 400
    /// - [`CredSource::Static`] (and BYO enabled) → ephemeral S3 backend signed
    ///   with the user's keys directly
    /// - [`CredSource::AssumeRole`] (and an assumer wired) → assume the role with
    ///   the server's own identity via STS, then build the ephemeral backend from
    ///   the minted credentials
    /// - [`CredSource::Default`] → the server's default backend
    ///
    /// When BYO is disabled (local-dir mode) any supplied credentials are
    /// ignored and the default backend is used. A role-arn request against a
    /// server with no assumer wired is a 400 (the feature is off here).
    ///
    /// The ephemeral client is pinned to the region carried on the request (the
    /// `x-dial9-aws-region` header or `aws_region` query param). A cross-region
    /// bucket therefore requires the correct region to ride along — the UI
    /// detects it once via `/api/credentials/check` and then keeps it in the
    /// stored credentials and the URL, so every subsequent request carries it.
    /// A request that reaches the wrong regional endpoint fails with
    /// [`StorageError::WrongRegion`] rather than an opaque error.
    ///
    /// [`StorageError::WrongRegion`]: crate::storage::StorageError::WrongRegion
    pub async fn resolve(
        &self,
        creds: MaybeCreds,
    ) -> Result<Arc<dyn StorageBackend>, (StatusCode, String)> {
        let parsed = match creds.0 {
            Ok(parsed) => parsed,
            Err(
                e @ (CredError::Incomplete
                | CredError::Malformed
                | CredError::InvalidRegion
                | CredError::ConflictingCredentials
                | CredError::InvalidRoleArn),
            ) => {
                return Err((StatusCode::BAD_REQUEST, e.message().to_string()));
            }
        };

        match parsed {
            CredSource::Static(temp) if self.allow_byo_creds => {
                self.log_chosen_identity(&temp, "bring-your-own credentials");
                Ok(self.ephemeral_backend(temp))
            }
            CredSource::AssumeRole { role_arn, region } if self.allow_byo_creds => {
                let temp = self.assume(&role_arn, region.as_deref()).await?;
                self.log_chosen_identity(&temp, "assumed-role credentials");
                Ok(self.ephemeral_backend(temp))
            }
            // BYO disabled (local-dir) — credentials are meaningless here.
            CredSource::Static(_) | CredSource::AssumeRole { .. } => Ok(self.backend.clone()),
            CredSource::Default => {
                // No credential headers reached the backend. On a BYO-capable
                // server this means we fall back to the server's ambient identity
                // — the usual cause of a "wrong account" error.
                if self.allow_byo_creds {
                    tracing::debug!(
                        "no x-dial9-aws-* credentials on request; using server's default identity"
                    );
                }
                Ok(self.backend.clone())
            }
        }
    }

    /// Assume `role_arn` (with the server's own identity) and return the minted
    /// credentials. Shared by `resolve` and the `/api/credentials/check` handler
    /// so the single assume-and-map-to-error policy can't drift between them:
    ///
    /// - no assumer wired → 400 (the feature is off here; never silently fall
    ///   back to the ambient identity, which would read the *wrong* account).
    /// - STS failure → 401 with a generic body; the concrete cause (which can
    ///   name the role/account) is logged server-side, never reflected.
    pub(crate) async fn assume(
        &self,
        role_arn: &credentials::RoleArn,
        region: Option<&str>,
    ) -> Result<credentials::TempCredentials, (StatusCode, String)> {
        let Some(assumer) = &self.role_assumer else {
            return Err((
                StatusCode::BAD_REQUEST,
                "this server does not support assume-role credentials".to_string(),
            ));
        };
        tracing::info!(role_arn = %role_arn.as_str(), "assuming role for request");
        assumer.assume_role(role_arn, region).await.map_err(|e| {
            tracing::warn!(role_arn = %role_arn.as_str(), error = %e, "assume-role failed");
            (
                StatusCode::UNAUTHORIZED,
                "could not assume the requested role".to_string(),
            )
        })
    }

    /// Build an ephemeral S3 backend from temporary credentials (shared by the
    /// BYOC and assume-role paths — both end here once they hold creds).
    fn ephemeral_backend(&self, temp: credentials::TempCredentials) -> Arc<dyn StorageBackend> {
        Arc::new(S3Backend::from_credentials(
            temp.credentials,
            temp.region.as_deref(),
            &self.ephemeral_s3,
        ))
    }

    /// Log which identity served the request — the access-key-id PREFIX only,
    /// never the secret/token — so it is unambiguous in the logs whether the
    /// user's keys, an assumed role, or the server's ambient identity made the
    /// S3 call.
    fn log_chosen_identity(&self, temp: &credentials::TempCredentials, via: &str) {
        let akid_prefix: String = temp.credentials.access_key_id().chars().take(8).collect();
        tracing::info!(
            akid_prefix = %akid_prefix,
            region = temp.region.as_deref().unwrap_or("(default)"),
            "using {via} for request"
        );
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

    Router::new()
        .route("/config", axum::routing::get(config::get_config))
        .route("/buckets", axum::routing::get(buckets::list_buckets))
        .route(
            "/credentials/check",
            axum::routing::post(check::check_credentials),
        )
        .route("/prefixes", axum::routing::get(prefixes::list_prefixes))
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
        .route("/uploaded/{id}", axum::routing::get(upload::get_uploaded))
        .merge(upload_route)
        // Permissive CORS so a page on another origin can POST a trace and read
        // it back via fetch(); also answers the OPTIONS preflight automatically.
        .layer(CorsLayer::permissive())
        // Per-request metrics. Layered on the API router (not the outer one) so
        // it sees the populated `MatchedPath` and only counts API requests, not
        // static-asset fetches. Publishes to the global `ServiceMetrics` sink.
        .layer(axum::middleware::from_fn(metrics::record_request_metrics))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

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
