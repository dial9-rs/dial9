use super::AppState;
use super::credentials::MaybeCreds;
use crate::storage::StorageBackend;
use axum::Router;
use axum::http::StatusCode;
use std::sync::Arc;

impl AppState {
    /// Always false because assuming a role requires S3 support.
    pub(crate) fn supports_assume_role(&self) -> bool {
        false
    }

    /// Return the configured local backend. AWS credential headers are ignored
    /// when the viewer is built without S3 support.
    pub async fn resolve(
        &self,
        _creds: MaybeCreds,
    ) -> Result<Arc<dyn StorageBackend>, (StatusCode, String)> {
        Ok(self.backend.clone())
    }

    pub(super) async fn resolve_with_region(
        &self,
        _creds: MaybeCreds,
        _ambient_region: Option<&str>,
    ) -> Result<Arc<dyn StorageBackend>, (StatusCode, String)> {
        Ok(self.backend.clone())
    }
}

pub(super) fn add_routes(router: Router<AppState>) -> Router<AppState> {
    router
}
