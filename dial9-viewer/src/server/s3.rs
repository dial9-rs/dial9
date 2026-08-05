use super::credentials::{self, CredError, CredSource, MaybeCreds};
use super::{AppState, check};
use crate::storage::{EphemeralS3Config, S3Backend, StorageBackend};
use axum::Router;
use axum::http::StatusCode;
use std::sync::Arc;

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

impl AppState {
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

    /// Whether this server will assume a caller-named role ARN.
    pub(crate) fn supports_assume_role(&self) -> bool {
        self.role_assumer.is_some()
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
        self.resolve_with_region(creds, None).await
    }

    /// Resolve a backend for aggregate reads, honoring a deep-linked region even
    /// when the request uses the server's ambient credentials. Explicit BYO or
    /// assumed-role credentials already carry their own validated region.
    pub(super) async fn resolve_with_region(
        &self,
        creds: MaybeCreds,
        ambient_region: Option<&str>,
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
                if self.allow_byo_creds {
                    if let Some(region) = ambient_region {
                        tracing::debug!(%region, "using ambient credentials in request region");
                        return Ok(Arc::new(S3Backend::from_env_in_region(region).await));
                    }
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

pub(super) fn add_routes(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/credentials/check",
        axum::routing::post(check::check_credentials),
    )
}
