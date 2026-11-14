use super::credentials::{self, CredError, CredSource, MaybeCreds};
use super::{AppState, ServerSourceIdentity, SourceIdentity, check};
use crate::ingest::cache_namespace::CacheNamespace;
use crate::storage::{EphemeralS3Config, S3Backend, StorageBackend};
use axum::Router;
use axum::http::StatusCode;
use dial9_core::rate_limited;
use std::sync::Arc;

/// The client-facing rejection when a request's AWS account cannot be verified.
/// Deliberately generic: the cause can name accounts or principals.
fn identity_unverified(status: StatusCode) -> (StatusCode, String) {
    (
        status,
        "could not verify the AWS account for this request".to_string(),
    )
}

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
            .with_source_identity(ServerSourceIdentity::Fixed(CacheNamespace::ambient()))
    }

    /// Inject ephemeral-S3 plumbing (test seam; production leaves this unset).
    #[doc(hidden)]
    pub fn with_ephemeral_s3(mut self, cfg: EphemeralS3Config) -> Self {
        self.ephemeral_s3 = Some(cfg);
        self
    }

    /// Replace the STS caller-identity lookup that namespaces the aggregate
    /// cache (test seam; production uses `StsCallerIdentityResolver`).
    #[doc(hidden)]
    pub fn with_caller_identity_resolver(
        mut self,
        resolver: Arc<dyn credentials::CallerIdentityResolver>,
    ) -> Self {
        self.caller_identity = resolver;
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
        Ok(self.resolve_source(creds, None).await?.0)
    }

    /// Resolve a backend for aggregate reads, honoring a deep-linked region even
    /// when the request uses the server's ambient credentials. Explicit BYO or
    /// assumed-role credentials already carry their own validated region.
    ///
    /// Also returns the identity that signs the backend's reads, so aggregation
    /// can namespace its cache by it. Resolving that identity to an account is
    /// deferred to [`Self::cache_namespace`], so browse routes make no extra
    /// STS call. The returned backend cannot reach the persistent aggregate
    /// cache (see [`AppState::guard_source`](super::AppState::guard_source)).
    pub(super) async fn resolve_source(
        &self,
        creds: MaybeCreds,
        ambient_region: Option<&str>,
    ) -> Result<(Arc<dyn StorageBackend>, SourceIdentity), (StatusCode, String)> {
        let (backend, identity) = self.resolve_unguarded(creds, ambient_region).await?;
        Ok((self.guard_source(backend), identity))
    }

    async fn resolve_unguarded(
        &self,
        creds: MaybeCreds,
        ambient_region: Option<&str>,
    ) -> Result<(Arc<dyn StorageBackend>, SourceIdentity), (StatusCode, String)> {
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
                let backend = self.ephemeral_backend(temp.clone());
                Ok((backend, SourceIdentity::Credentials(temp)))
            }
            CredSource::AssumeRole { role_arn, region } if self.allow_byo_creds => {
                let temp = self.assume(&role_arn, region.as_deref()).await?;
                self.log_chosen_identity(&temp, "assumed-role credentials");
                // The role ARN names the account whose credentials STS just
                // minted, so it needs no further identity lookup.
                let Some(namespace) =
                    CacheNamespace::aws_account(role_arn.partition(), role_arn.account_id())
                else {
                    tracing::warn!(role_arn = %role_arn.as_str(), "assumed role has no valid account");
                    return Err(identity_unverified(StatusCode::INTERNAL_SERVER_ERROR));
                };
                Ok((
                    self.ephemeral_backend(temp),
                    SourceIdentity::Known(namespace),
                ))
            }
            // BYO disabled (local-dir) — credentials are meaningless here, so
            // they select neither the backend nor its identity.
            CredSource::Static(_) | CredSource::AssumeRole { .. } => {
                Ok((self.backend.clone(), self.source_identity.of_backend()))
            }
            CredSource::Default => {
                if self.require_explicit_source_identity {
                    tracing::info!("rejecting source read without explicit AWS identity");
                    return Err((
                        StatusCode::BAD_REQUEST,
                        check::CREDENTIALS_REQUIRED.to_string(),
                    ));
                }
                if self.allow_byo_creds {
                    // Empty is absent, as in the credential parser.
                    if let Some(region) = ambient_region.filter(|region| !region.is_empty()) {
                        // The parser ignores this query value when a header
                        // region takes precedence, so validate it before it
                        // selects the S3 endpoint.
                        if !credentials::is_valid_region(region) {
                            return Err((
                                StatusCode::BAD_REQUEST,
                                CredError::InvalidRegion.message().to_string(),
                            ));
                        }
                        tracing::debug!(%region, "using ambient credentials in request region");
                        let backend = Arc::new(S3Backend::from_env_in_region(region).await);
                        let identity = SourceIdentity::Known(CacheNamespace::ambient());
                        return Ok((backend, identity));
                    }
                    tracing::debug!(
                        "no x-dial9-aws-* credentials on request; using server's default identity"
                    );
                }
                Ok((self.backend.clone(), self.source_identity.of_backend()))
            }
        }
    }

    /// The cache namespace for `identity`. Raw credentials cost one STS
    /// `GetCallerIdentity` call; other identities, including the ambient chain,
    /// are known.
    ///
    /// Fails closed: when the account cannot be verified, the request is
    /// rejected with a generic message rather than falling back to a shared
    /// namespace. Details are logged server-side. Only STS rejecting supplied
    /// credentials is a 401; any other failure is a 503.
    pub(super) async fn cache_namespace(
        &self,
        identity: SourceIdentity,
    ) -> Result<CacheNamespace, (StatusCode, String)> {
        let temp = match &identity {
            SourceIdentity::Known(namespace) => return Ok(namespace.clone()),
            SourceIdentity::Credentials(temp) => temp,
        };
        let caller = match self
            .caller_identity
            .caller_identity(temp, temp.region.as_deref())
            .await
        {
            Ok(caller) => caller,
            Err(e) => {
                rate_limited!(std::time::Duration::from_secs(60), {
                    tracing::info!(
                        error = %e,
                        "could not resolve the AWS account of bring-your-own credentials"
                    );
                });
                return Err(identity_unverified(if e.is_rejected() {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                }));
            }
        };
        CacheNamespace::from_caller_identity(caller.account(), caller.arn()).ok_or_else(|| {
            rate_limited!(std::time::Duration::from_secs(60), {
                tracing::warn!(
                    account = %caller.account(),
                    arn = %caller.arn(),
                    "caller identity has no valid AWS account"
                );
            });
            identity_unverified(StatusCode::SERVICE_UNAVAILABLE)
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::aggregate::AggContext;
    use crate::server::credentials::{
        CallerIdentity, CallerIdentityError, CallerIdentityResolver, TempCredentials,
    };
    use std::sync::Mutex;

    const SUPPLIED_ACCOUNT: &str = "999999999999";

    /// One recorded lookup's region.
    type Lookup = Option<String>;

    /// Reports [`SUPPLIED_ACCOUNT`] for every lookup.
    #[derive(Default)]
    struct FakeResolver {
        lookups: Mutex<Vec<Lookup>>,
    }

    impl FakeResolver {
        fn lookups(&self) -> Vec<Lookup> {
            self.lookups.lock().unwrap().clone()
        }
    }

    impl CallerIdentityResolver for FakeResolver {
        fn caller_identity<'a>(
            &'a self,
            _credentials: &'a TempCredentials,
            region: Option<&'a str>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<CallerIdentity, CallerIdentityError>>
                    + Send
                    + 'a,
            >,
        > {
            self.lookups
                .lock()
                .unwrap()
                .push(region.map(str::to_string));
            let account = SUPPLIED_ACCOUNT;
            let result = Ok(CallerIdentity::new(
                account,
                format!("arn:aws:sts::{account}:assumed-role/server/s"),
            ));
            Box::pin(async move { result })
        }
    }

    fn state(
        identity: ServerSourceIdentity,
        allow_byo: bool,
        resolver: Arc<FakeResolver>,
    ) -> AppState {
        let backend: Arc<dyn StorageBackend> =
            Arc::new(crate::storage::LocalBackend::new_temporary_aggregate());
        let agg = AggContext {
            source: Arc::clone(&backend),
            output: Arc::clone(&backend),
            source_bucket: "src".into(),
            source_is_local: false,
            output_bucket: "out".into(),
            output_prefix: "base".into(),
            source_prefixes: vec![String::new()],
            segment_duration_secs: 60,
        };
        AppState::new(backend, Some("src".into()), None)
            .with_agg(agg)
            .with_byo_creds(allow_byo)
            .with_source_identity(identity)
            .with_caller_identity_resolver(resolver)
    }

    fn supplied_credentials() -> MaybeCreds {
        MaybeCreds(Ok(CredSource::Static(TempCredentials::new(
            "AKID",
            "secret",
            None,
            Some("eu-west-1".into()),
        ))))
    }

    fn no_credentials() -> MaybeCreds {
        MaybeCreds(Ok(CredSource::Default))
    }

    /// The namespace and lookup follow the credentials that sign source reads.
    /// Request credentials select neither the configured context's backend nor
    /// its namespace, disabled BYO credentials select nothing either, and every
    /// lookup carries the request's validated region.
    #[rstest::rstest]
    #[case::configured_ambient(
        ServerSourceIdentity::Fixed(CacheNamespace::ambient()),
        true,
        None,
        supplied_credentials(),
        None,
        "base/account=ambient",
        vec![]
    )]
    #[case::byo_disabled(
        ServerSourceIdentity::Fixed(CacheNamespace::simulator()),
        false,
        Some("src"),
        supplied_credentials(),
        None,
        "flamegraph-data/account=simulator",
        vec![]
    )]
    #[case::byo_enabled(
        ServerSourceIdentity::Fixed(CacheNamespace::supplied()),
        true,
        Some("src"),
        supplied_credentials(),
        None,
        "flamegraph-data/account=aws.999999999999",
        vec![Some("eu-west-1".to_string())]
    )]
    #[case::ambient_in_region(
        ServerSourceIdentity::Fixed(CacheNamespace::supplied()),
        true,
        Some("src"),
        no_credentials(),
        Some("us-west-2"),
        "flamegraph-data/account=ambient",
        vec![]
    )]
    #[tokio::test]
    async fn namespace_follows_the_signing_credentials(
        #[case] identity: ServerSourceIdentity,
        #[case] allow_byo: bool,
        #[case] bucket: Option<&str>,
        #[case] creds: MaybeCreds,
        #[case] aws_region: Option<&str>,
        #[case] want: &str,
        #[case] lookups: Vec<Lookup>,
    ) {
        let resolver = Arc::new(FakeResolver::default());
        let agg = state(identity, allow_byo, Arc::clone(&resolver))
            .agg_context_for(bucket, None, aws_region, creds)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(agg.output_prefix, want);
        assert_eq!(resolver.lookups(), lookups);
    }

    /// An invalid ambient `aws_region` is rejected before it can select an S3
    /// endpoint.
    #[tokio::test]
    async fn invalid_ambient_region_is_rejected_before_any_lookup() {
        let resolver = Arc::new(FakeResolver::default());
        let error = state(
            ServerSourceIdentity::Fixed(CacheNamespace::supplied()),
            true,
            Arc::clone(&resolver),
        )
        .agg_context_for(
            Some("src"),
            None,
            Some("us-east-1.attacker.example"),
            no_credentials(),
        )
        .await
        .err()
        .expect("invalid region must fail");
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(resolver.lookups().is_empty());
    }
}
