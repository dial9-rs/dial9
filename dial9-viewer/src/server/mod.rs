use crate::storage::{EphemeralS3Config, S3Backend, StorageBackend};
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
/// Shared by startup region detection ([`crate::serve`]) and the
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
    /// Output prefix for BYOC aggregation. Defaults to "flamegraph-data".
    pub agg_output_prefix: String,
    /// Output bucket for BYOC aggregation. `None` means write the aggregated
    /// part-files back into the source bucket (the query-param `bucket`). Set it
    /// (via `--agg-output-bucket`) when the source bucket is read-only, so the
    /// output lands in a separate writable bucket.
    pub agg_output_bucket: Option<String>,
    /// Region-aware backend for [`Self::agg_output_bucket`], built once at
    /// startup (the output bucket name is known then, so no per-request region
    /// detection). Writes to the output bucket use the server's ambient identity
    /// — the operator controls that bucket via `--agg-output-bucket`. `None`
    /// when no output bucket override is configured; the BYOC path then writes
    /// to the source bucket through the request's own (BYOC or ambient) backend.
    pub agg_output_backend: Option<Arc<dyn StorageBackend>>,
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
            agg_output_prefix: "flamegraph-data".to_string(),
            agg_output_bucket: None,
            agg_output_backend: None,
            agg_segment_secs: crate::ingest::aggregate::DEFAULT_SEGMENT_DURATION_SECS,
            fold_limits: crate::ingest::aggregate::FoldLimits::default(),
        }
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

    pub fn with_agg_output_prefix(mut self, prefix: String) -> Self {
        self.agg_output_prefix = prefix;
        self
    }

    /// Set the output bucket for BYOC aggregation, paired with the region-aware
    /// backend that writes to it. Pass `None` to keep the default of writing
    /// back into the source bucket through the request's own backend.
    pub fn with_agg_output_bucket(
        mut self,
        bucket: Option<String>,
        backend: Option<Arc<dyn StorageBackend>>,
    ) -> Self {
        self.agg_output_bucket = bucket;
        self.agg_output_backend = backend;
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
    /// configured `--agg-output-bucket` (through its own region-aware backend) or
    /// else back into the source bucket. When `bucket` is `None`, fall back to
    /// the server's `--agg` context if one is configured.
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
            // Output may target a different, writable bucket than the (often
            // read-only) source. When `--agg-output-bucket` is configured we
            // write there through its own region-aware backend; otherwise we
            // write back into the source bucket through the request's backend.
            let (output_bucket, output) = match (&self.agg_output_bucket, &self.agg_output_backend)
            {
                (Some(out_bucket), Some(out_backend)) => {
                    (out_bucket.clone(), Arc::clone(out_backend))
                }
                _ => (bucket.to_string(), Arc::clone(&backend)),
            };
            tracing::info!(
                %bucket,
                %output_bucket,
                resolved_source_prefix = %source_prefix,
                output_prefix = %self.agg_output_prefix,
                "agg: BYOC context"
            );
            Ok(Some(AggContext {
                source: backend,
                output,
                source_bucket: bucket.to_string(),
                source_is_local: false,
                output_bucket,
                output_prefix: self.agg_output_prefix.clone(),
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
                    tracing::info!(
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
        .with_state(state)
}
