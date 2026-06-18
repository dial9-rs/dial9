use crate::storage::StorageBackend;
use axum::Router;
use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

mod config;
mod prefixes;
mod search;
mod trace;
mod upload;

pub use upload::{UploadLimits, UploadStore};

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
        .route("/prefixes", axum::routing::get(prefixes::list_prefixes))
        .route("/search", axum::routing::get(search::search))
        .route("/trace", axum::routing::get(trace::get_trace))
        .route("/uploaded/{id}", axum::routing::get(upload::get_uploaded))
        .merge(upload_route)
        // Permissive CORS so a page on another origin can POST a trace and read
        // it back via fetch(); also answers the OPTIONS preflight automatically.
        .layer(CorsLayer::permissive())
        .with_state(state)
}
