use super::AppState;
use super::SourceIdentity;
use super::credentials::MaybeCreds;
use crate::ingest::cache_namespace::CacheNamespace;
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
        creds: MaybeCreds,
    ) -> Result<Arc<dyn StorageBackend>, (StatusCode, String)> {
        Ok(self.resolve_source(creds, None).await?.0)
    }

    /// The configured backend and its fixed identity; credential headers are
    /// ignored, so they cannot select a cache namespace either. The backend
    /// cannot reach a persistent aggregate cache (see
    /// [`AppState::guard_source`](super::AppState::guard_source)).
    pub(super) async fn resolve_source(
        &self,
        _creds: MaybeCreds,
        _ambient_region: Option<&str>,
    ) -> Result<(Arc<dyn StorageBackend>, SourceIdentity), (StatusCode, String)> {
        Ok((
            self.guard_source(self.backend.clone()),
            self.source_identity.of_backend(),
        ))
    }

    /// Without S3 support every identity is a fixed namespace.
    pub(super) async fn cache_namespace(
        &self,
        identity: SourceIdentity,
    ) -> Result<CacheNamespace, (StatusCode, String)> {
        let SourceIdentity::Known(namespace) = identity;
        Ok(namespace)
    }
}

pub(super) fn add_routes(router: Router<AppState>) -> Router<AppState> {
    router
}
