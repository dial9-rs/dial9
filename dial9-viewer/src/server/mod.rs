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

mod buckets;
mod check;
mod config;
pub mod credentials;
mod error;
mod prefixes;
mod search;
mod trace;
mod upload;

pub use upload::{UploadLimits, UploadStore};

use credentials::{CredError, MaybeCreds};

/// Detect a bucket's region via `HeadBucket`, reading `bucket_region()` on
/// success and the `x-amz-bucket-region` response header on the redirect error
/// that S3 returns when the client's region doesn't match the bucket's.
///
/// Shared by startup region detection ([`crate::serve`]) and the
/// `/api/credentials/check` endpoint.
pub async fn region_from_head_bucket(client: &aws_sdk_s3::Client, bucket: &str) -> Option<String> {
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
    /// In-memory store of temporary, POSTed traces. `None` (the default) means
    /// the trace-upload feature is disabled and its routes are not registered.
    pub uploads: Option<Arc<UploadStore>>,
    /// Whether the UI should offer the bring-your-own-credentials panel and
    /// whether handlers honor `x-dial9-aws-*` headers. True for S3 backends,
    /// false for `--local-dir` (the data is local; credentials are meaningless).
    pub allow_byo_creds: bool,
    /// Optional plumbing for ephemeral S3 client construction (test injection
    /// of the in-process fake; `None` in production → default HTTPS connector).
    #[doc(hidden)]
    pub ephemeral_s3: Option<EphemeralS3Config>,
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
            uploads: None,
            allow_byo_creds: false,
            ephemeral_s3: None,
        }
    }

    pub fn with_dev_ui_dir(mut self, dir: PathBuf) -> Self {
        self.dev_ui_dir = Some(dir);
        self
    }

    /// Enable the temporary trace-upload feature with the given caps. Without
    /// this, `POST /api/upload` and `GET /api/uploaded/{id}` are not registered.
    pub fn with_uploads(mut self, limits: UploadLimits) -> Self {
        self.uploads = Some(Arc::new(UploadStore::new(limits)));
        self
    }

    /// Enable the bring-your-own-credentials path (S3 backends only).
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

    /// Pick the storage backend for a request given any supplied credentials.
    ///
    /// Bringing credentials is always optional:
    /// - incomplete/malformed credentials → 400
    /// - credentials present (and BYO enabled) → ephemeral S3 backend built
    ///   from those credentials
    /// - no credentials → the server's default backend
    ///
    /// When BYO is disabled (local-dir mode) any supplied credentials are
    /// ignored and the default backend is used.
    pub fn resolve(
        &self,
        creds: MaybeCreds,
    ) -> Result<Arc<dyn StorageBackend>, (StatusCode, String)> {
        let parsed = match creds.0 {
            Ok(parsed) => parsed,
            Err(e @ (CredError::Incomplete | CredError::Malformed | CredError::InvalidRegion)) => {
                return Err((StatusCode::BAD_REQUEST, e.message().to_string()));
            }
        };

        match parsed {
            Some(temp) if self.allow_byo_creds => {
                // Log which identity served the request (akid prefix only — never
                // the secret/token) so it's unambiguous whether the user's pasted
                // credentials or the server's ambient identity made the S3 call.
                let akid = temp.credentials.access_key_id();
                let akid_prefix: String = akid.chars().take(8).collect();
                tracing::info!(
                    akid_prefix = %akid_prefix,
                    region = temp.region.as_deref().unwrap_or("(default)"),
                    "using bring-your-own credentials for request"
                );
                Ok(Arc::new(S3Backend::from_credentials(
                    temp.credentials,
                    temp.region.as_deref(),
                    &self.ephemeral_s3,
                )))
            }
            // BYO disabled (local-dir) — credentials are meaningless here.
            Some(_) => Ok(self.backend.clone()),
            None => {
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
        .route("/search", axum::routing::get(search::search))
        .route("/object", axum::routing::get(trace::get_object))
        // DEPRECATED (slated for removal): superseded by /object, which serves a
        // single object's raw bytes so the browser merges/gunzips client-side.
        .route("/trace", axum::routing::get(trace::get_trace))
        .route("/uploaded/{id}", axum::routing::get(upload::get_uploaded))
        .merge(upload_route)
        // Permissive CORS so a page on another origin can POST a trace and read
        // it back via fetch(); also answers the OPTIONS preflight automatically.
        .layer(CorsLayer::permissive())
        .with_state(state)
}
