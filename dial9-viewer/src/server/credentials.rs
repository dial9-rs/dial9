//! Per-request AWS credential resolution for S3 reads. A request can supply its
//! identity one of two ways, never both:
//!
//!   1. **Bring-your-own-credentials (BYOC)** — temporary credentials in the
//!      `x-dial9-aws-{access-key-id,secret-access-key,session-token}` headers.
//!      The backend signs S3 with exactly those; it never holds standing access.
//!   2. **Assume-role** — a role ARN in the `x-dial9-aws-role-arn` header. The
//!      backend assumes that role with its OWN ambient identity (STS) and signs
//!      S3 with the minted temporary credentials. This is the secondary path
//!      for a deployment that *has* credentials (an instance/task role allowed
//!      to assume the target's reader role) rather than receiving them.
//!
//! Both transports are per-request (no server-side session store). The
//! [`MaybeCreds`] extractor parses the headers into a [`CredSource`];
//! [`crate::server::AppState::resolve`] turns that into an ephemeral
//! [`crate::storage::S3Backend`] — minting credentials via STS first for the
//! assume-role variant.

use aws_sdk_s3::config::Credentials;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use std::convert::Infallible;

/// Header names. Kept as constants so the `/api/credentials/check` handler and
/// any future tooling reference the exact same strings.
pub const HEADER_ACCESS_KEY_ID: &str = "x-dial9-aws-access-key-id";
pub const HEADER_SECRET_ACCESS_KEY: &str = "x-dial9-aws-secret-access-key";
pub const HEADER_SESSION_TOKEN: &str = "x-dial9-aws-session-token";
pub const HEADER_REGION: &str = "x-dial9-aws-region";
/// Role ARN the server assumes (with its own identity) to mint read credentials
/// — the assume-role alternative to supplying BYOC keys directly. Carries an
/// optional region via the shared [`HEADER_REGION`].
pub const HEADER_ROLE_ARN: &str = "x-dial9-aws-role-arn";

/// Query-parameter names for the assume-role path, so a read is **linkable**:
/// `?aws_role_arn=…&aws_region=…` names a role to assume (and the region to
/// reach the bucket in). A role ARN grants nothing on its own — the server's
/// own identity must be allowed to assume it — so it is safe to put in a URL,
/// exactly as the existing deep-link design carries `cl_acct`/`cl_role`.
///
/// Only the assume-role path is exposed via query params; the BYOC static keys
/// (secret/token) are header-only and MUST NOT be linkable, because a URL is
/// shareable/logged/cached and a secret access key in one is a credential leak.
pub const QUERY_ROLE_ARN: &str = "aws_role_arn";
pub const QUERY_REGION: &str = "aws_region";

/// STS session name attached to the assume-role call, so reads through this
/// path are attributable in the target account's CloudTrail.
pub const ASSUME_ROLE_SESSION_NAME: &str = "dial9-viewer";

/// Provider name attached to the SDK [`Credentials`] we build. Surfaces in SDK
/// diagnostics so BYO credentials are distinguishable from ambient ones.
const PROVIDER_NAME: &str = "dial9-byo";

/// User-supplied temporary credentials plus an optional region.
///
/// The credentials are stored in the AWS SDK's own [`Credentials`] type rather
/// than loose fields: it is exactly what we hand to `.credentials_provider(..)`,
/// it natively carries an optional expiry for temporary credentials, and there
/// is no second representation to keep in sync.
// `aws_sdk_s3::config::Credentials` derives a redacting `Debug` (it never prints
// the secret/token), so deriving here is safe — needed for `CredSource: Debug`.
#[derive(Clone, Debug)]
pub struct TempCredentials {
    pub credentials: Credentials,
    /// Region resolved by `/api/credentials/check` (or supplied directly).
    /// `None` falls back to a default at client-build time.
    pub region: Option<String>,
}

impl TempCredentials {
    /// Build from raw header values. `session_token`/`region` are optional.
    pub fn new(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<String>,
        region: Option<String>,
    ) -> Self {
        Self {
            // A concrete `Credentials` value is a *static* provider: it can never
            // fall back to IMDS/env, which is the security guarantee we want.
            credentials: Credentials::new(
                access_key_id,
                secret_access_key,
                session_token,
                None, // expiry — temporary creds simply fail at S3 once expired
                PROVIDER_NAME,
            ),
            region,
        }
    }
}

/// A validated `arn:aws:iam::<account>:role/<name>` role ARN. The newtype makes
/// "this string passed [`is_valid_role_arn`]" a type-level fact, so the resolve
/// path can hand it to STS without re-checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleArn(String);

impl RoleArn {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How a request asked us to authenticate its S3 access. Exactly one variant is
/// chosen per request; the two credentialed variants are mutually exclusive
/// (see [`parse_cred_inputs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredSource {
    /// No credential headers — use the server's default (ambient) backend.
    Default,
    /// BYOC: sign S3 directly with these user-supplied temporary credentials.
    Static(TempCredentials),
    /// Assume this role with the server's own identity, then sign S3 with the
    /// minted credentials. `region` (from [`HEADER_REGION`]) pins the resulting
    /// S3 client when present.
    AssumeRole {
        role_arn: RoleArn,
        region: Option<String>,
    },
}

// TempCredentials needs PartialEq/Eq only so CredSource can derive them (used in
// tests). aws Credentials doesn't impl Eq, so compare the fields we set.
impl PartialEq for TempCredentials {
    fn eq(&self, other: &Self) -> bool {
        self.credentials.access_key_id() == other.credentials.access_key_id()
            && self.credentials.secret_access_key() == other.credentials.secret_access_key()
            && self.credentials.session_token() == other.credentials.session_token()
            && self.region == other.region
    }
}
impl Eq for TempCredentials {}

/// Why credential headers could not be turned into a [`CredSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredError {
    /// Exactly one of access-key-id / secret-access-key was provided. We never
    /// silently ignore half a credential — that hides a configuration bug.
    Incomplete,
    /// A header value was not valid UTF-8 / not a valid header string.
    Malformed,
    /// The supplied region was not a syntactically valid AWS region name. The
    /// region is interpolated into the S3 endpoint host, so we constrain it to
    /// the AWS region charset rather than feeding arbitrary input to endpoint
    /// resolution.
    InvalidRegion,
    /// Both BYOC keys and a role ARN were supplied. They are alternatives — one
    /// signs directly, the other mints via STS — so requiring exactly one keeps
    /// the chosen identity unambiguous rather than silently preferring one.
    ConflictingCredentials,
    /// The role ARN was not a syntactically valid IAM role ARN. Like the region
    /// check, this is a defense-in-depth syntactic gate before the value reaches
    /// the STS client.
    InvalidRoleArn,
}

impl CredError {
    pub fn message(&self) -> &'static str {
        match self {
            CredError::Incomplete => {
                "incomplete credentials: both access key id and secret access key are required"
            }
            CredError::Malformed => "malformed credential header",
            CredError::InvalidRegion => "invalid region",
            CredError::ConflictingCredentials => {
                "supply either bring-your-own credentials or a role ARN, not both"
            }
            CredError::InvalidRoleArn => "invalid role ARN",
        }
    }
}

/// Whether `arn` is a syntactically valid IAM role ARN:
/// `arn:aws:iam::<account>:role/<path-and-name>`. We accept any partition
/// (`aws`, `aws-cn`, `aws-us-gov`), require the `iam` service with an empty
/// region field (IAM is global), a 12-digit account id, and a non-empty
/// `role/...` resource. This is a syntactic gate, not an existence check — STS
/// is the authority on whether the role exists and is assumable; we only keep
/// obviously-malformed input out of the SDK call.
fn is_valid_role_arn(arn: &str) -> bool {
    // Bound the input before parsing — AWS caps an ARN at 2048 chars, and this
    // keeps a pathological header/query value out of the SDK (mirrors the ≤40
    // cap on region).
    if arn.is_empty() || arn.len() > 2048 {
        return false;
    }
    // arn : partition : service : region : account : resource
    // IAM ARNs put nothing in the region field and `iam` in the service field.
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() != 6 {
        return false;
    }
    let [prefix, partition, service, region, account, resource] = parts[..] else {
        return false;
    };
    if prefix != "arn" {
        return false;
    }
    if !matches!(partition, "aws" | "aws-cn" | "aws-us-gov") {
        return false;
    }
    if service != "iam" || !region.is_empty() {
        return false;
    }
    if account.len() != 12 || !account.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    // Resource must be a role: `role/<name>` or `role/<path>/<name>`, name
    // non-empty. (No wildcards — this names a single role to assume.)
    match resource.strip_prefix("role/") {
        Some(rest) => !rest.is_empty() && !rest.contains('*') && !rest.contains('?'),
        None => false,
    }
}

/// Whether `region` is a syntactically valid AWS region name: hyphen-separated
/// runs of lowercase alphanumerics, starting with a letter and ending with an
/// alphanumeric, ≤40 chars (e.g. `us-east-1`, `ap-southeast-2`, `us-gov-west-1`).
///
/// This is a defense-in-depth syntactic check, not an existence check — the
/// region is interpolated into the S3 endpoint host (`s3.{region}.amazonaws.com`
/// under the default resolver), so we reject anything outside the region shape
/// rather than relying on the SDK's endpoint rules to sanitize it. (There is no
/// standalone smithy region validator we can call — region validation lives
/// inside endpoint resolution, not as a public function.) Beyond the charset we
/// require a leading letter, a non-hyphen final character, and no consecutive
/// hyphens, so degenerate inputs like `-`, `us--east-`, or `us-east-` are
/// rejected.
fn is_valid_region(region: &str) -> bool {
    if region.is_empty() || region.len() > 40 {
        return false;
    }
    let bytes = region.as_bytes();
    // Must start with a lowercase letter and end with a lowercase alphanumeric.
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    if !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    // Body: only lowercase alphanumerics and single (non-consecutive) hyphens.
    let mut prev_hyphen = false;
    for &b in bytes {
        if b == b'-' {
            if prev_hyphen {
                return false;
            }
            prev_hyphen = true;
        } else if b.is_ascii_lowercase() || b.is_ascii_digit() {
            prev_hyphen = false;
        } else {
            return false;
        }
    }
    true
}

/// Infallible extractor over the request's credential inputs, yielding a
/// [`CredSource`] (or a [`CredError`] the handler maps to 400). Two transports
/// feed it:
///
///   - **headers** (`x-dial9-aws-*`): the full BYOC keys, session token, region,
///     and/or a role ARN.
///   - **query params** (`?aws_role_arn=…&aws_region=…`): the assume-role path
///     ONLY, so a role-based read is linkable. Static secret keys are never read
///     from the query string — a URL is shareable/logged and a secret in one is
///     a leak.
///
/// Resolution (see [`parse_cred_inputs`]):
///
/// - nothing supplied                          → `Ok(CredSource::Default)`
/// - akid + secret (+token) in headers          → `Ok(CredSource::Static(..))`
/// - role-arn (header or query)                 → `Ok(CredSource::AssumeRole)`
/// - exactly one of akid/secret                 → `Err(Incomplete)`
/// - BYOC keys AND any role-arn                  → `Err(ConflictingCredentials)`
/// - a role-arn in BOTH header and query         → `Err(ConflictingCredentials)`
///
/// The extractor is deliberately pure parsing and state-agnostic; the decision
/// of *what backend to build* (and the STS call for the assume-role variant)
/// lives in `AppState::resolve` where the server config is available.
pub struct MaybeCreds(pub Result<CredSource, CredError>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeCreds {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        Ok(MaybeCreds(parse_cred_inputs(
            &parts.headers,
            parts.uri.query(),
        )))
    }
}

/// The assume-role inputs pulled from the query string: a role ARN and/or a
/// region. Region alone is meaningless (it pins an S3 client that has no
/// non-default credentials), so it only takes effect alongside a role ARN.
struct QueryCreds {
    role_arn: Option<String>,
    region: Option<String>,
}

/// Parse the assume-role query params (`aws_role_arn`, `aws_region`) out of a
/// raw query string. Unknown params are ignored (the data endpoints carry their
/// own `bucket`/`prefix`/etc.). Empty values are treated as absent.
fn parse_query_creds(query: Option<&str>) -> QueryCreds {
    let mut role_arn = None;
    let mut region = None;
    if let Some(q) = query {
        for (k, v) in form_urlencoded::parse(q.as_bytes()) {
            match k.as_ref() {
                QUERY_ROLE_ARN if !v.is_empty() => role_arn = Some(v.into_owned()),
                QUERY_REGION if !v.is_empty() => region = Some(v.into_owned()),
                _ => {}
            }
        }
    }
    QueryCreds { role_arn, region }
}

/// Pull a [`CredSource`] from the request's headers and query string. Split from
/// the extractor so it can be unit-tested without constructing a request.
pub fn parse_cred_inputs(
    headers: &axum::http::HeaderMap,
    query: Option<&str>,
) -> Result<CredSource, CredError> {
    let get = |name: &str| -> Result<Option<String>, CredError> {
        match headers.get(name) {
            None => Ok(None),
            Some(v) => v
                .to_str()
                .map(|s| Some(s.to_string()))
                .map_err(|_| CredError::Malformed),
        }
    };

    let access_key_id = get(HEADER_ACCESS_KEY_ID)?;
    let secret_access_key = get(HEADER_SECRET_ACCESS_KEY)?;
    let session_token = get(HEADER_SESSION_TOKEN)?;
    let header_role_arn = get(HEADER_ROLE_ARN)?.filter(|s| !s.is_empty());
    let header_region = get(HEADER_REGION)?.filter(|s| !s.is_empty());

    let QueryCreds {
        role_arn: query_role_arn,
        region: query_region,
    } = parse_query_creds(query);

    // A role ARN may arrive via header OR query, never both — two different
    // sources naming a role is ambiguous, same as mixing BYOC with a role.
    let role_arn = match (header_role_arn, query_role_arn) {
        (Some(_), Some(_)) => return Err(CredError::ConflictingCredentials),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };

    // Region may come from either transport; the header wins when both are set
    // (it travels with the explicit BYOC headers). Validate wherever it lands —
    // it is interpolated into the S3 endpoint host.
    let region = match header_region.or(query_region) {
        Some(r) if !is_valid_region(&r) => return Err(CredError::InvalidRegion),
        other => other,
    };

    let has_byoc = access_key_id.is_some() || secret_access_key.is_some();
    // BYOC and assume-role are alternatives — reject the ambiguous combination
    // before interpreting either, so we never silently pick one.
    if has_byoc && role_arn.is_some() {
        return Err(CredError::ConflictingCredentials);
    }

    if let Some(arn) = role_arn {
        if !is_valid_role_arn(&arn) {
            return Err(CredError::InvalidRoleArn);
        }
        return Ok(CredSource::AssumeRole {
            role_arn: RoleArn(arn),
            region,
        });
    }

    match (access_key_id, secret_access_key) {
        (None, None) => Ok(CredSource::Default),
        (Some(akid), Some(secret)) => Ok(CredSource::Static(TempCredentials::new(
            akid,
            secret,
            session_token.filter(|s| !s.is_empty()),
            region,
        ))),
        // Exactly one half present — refuse rather than silently fall back.
        _ => Err(CredError::Incomplete),
    }
}

/// Mints credentials by assuming a role. Abstracted as a trait so the STS call
/// is an injectable seam: production uses [`StsRoleAssumer`]; tests inject a
/// fake that returns canned credentials without a network call (mirroring how
/// [`crate::storage::EphemeralS3Config`] injects the S3 connector).
pub trait RoleAssumer: Send + Sync {
    /// Assume `role_arn` and return temporary credentials. `region`, when set,
    /// pins both the STS endpoint used to assume the role and the region carried
    /// on the resulting credentials. Errors are opaque: the resolve path maps any
    /// failure to a 401 so the SDK message — which can name the role/account — is
    /// never reflected to the client (it is logged server-side instead).
    fn assume_role<'a>(
        &'a self,
        role_arn: &'a RoleArn,
        region: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TempCredentials, AssumeRoleError>> + Send + 'a>,
    >;
}

/// Opaque assume-role failure. Deliberately carries no detail across the trait
/// boundary — the concrete cause is logged at the call site, and the client
/// sees only a generic 401 (see [`crate::server::AppState::resolve`]).
#[derive(Debug)]
pub struct AssumeRoleError(pub String);

impl std::fmt::Display for AssumeRoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "assume-role failed: {}", self.0)
    }
}
impl std::error::Error for AssumeRoleError {}

/// Production [`RoleAssumer`]: mints credentials via [`AssumeRoleProvider`],
/// which assumes the role with the process's ambient identity (instance/task
/// role). Holds the ambient [`SdkConfig`] and builds a provider per request, so
/// the role ARN can vary per request.
///
/// Building a fresh provider each call forgoes [`AssumeRoleProvider`]'s
/// credential caching across requests — fine while reads are infrequent. When
/// that matters, cache `SharedCredentialsProvider`s here keyed by (ARN, region)
/// and reuse them: each then caches and refreshes its own credentials.
pub struct StsRoleAssumer {
    config: aws_config::SdkConfig,
}

impl StsRoleAssumer {
    /// Build from the ambient AWS config (the server's own identity is what does
    /// the assuming).
    pub async fn from_env() -> Self {
        Self::from_config(aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await)
    }

    /// Build from an explicit [`SdkConfig`] (test seam / custom base credentials
    /// + STS endpoint).
    pub fn from_config(config: aws_config::SdkConfig) -> Self {
        Self { config }
    }
}

impl RoleAssumer for StsRoleAssumer {
    fn assume_role<'a>(
        &'a self,
        role_arn: &'a RoleArn,
        region: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TempCredentials, AssumeRoleError>> + Send + 'a>,
    > {
        use aws_sdk_s3::config::ProvideCredentials;
        Box::pin(async move {
            let mut builder = aws_config::sts::AssumeRoleProvider::builder(role_arn.as_str())
                .configure(&self.config)
                .session_name(ASSUME_ROLE_SESSION_NAME);
            if let Some(region) = region {
                builder = builder.region(aws_sdk_s3::config::Region::new(region.to_string()));
            }
            let provider = builder.build().await;

            let creds = provider
                .provide_credentials()
                .await
                .map_err(|e| AssumeRoleError(format!("{e}")))?;

            Ok(TempCredentials::new(
                creds.access_key_id(),
                creds.secret_access_key(),
                creds.session_token().map(str::to_string),
                region.map(str::to_string),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    /// Header-only parse helper: the many header tests below predate query
    /// params and exercise that path with no query string.
    fn parse_cred_headers(headers: &HeaderMap) -> Result<CredSource, CredError> {
        parse_cred_inputs(headers, None)
    }

    /// Unwrap a parsed `CredSource::Static`, panicking on any other variant.
    fn expect_static(src: CredSource) -> TempCredentials {
        match src {
            CredSource::Static(t) => t,
            other => panic!("expected CredSource::Static, got {other:?}"),
        }
    }

    #[test]
    fn absent_headers_yield_default() {
        let parsed = parse_cred_headers(&HeaderMap::new()).unwrap();
        assert_eq!(parsed, CredSource::Default);
    }

    #[test]
    fn full_credentials_parse() {
        let h = headers(&[
            (HEADER_ACCESS_KEY_ID, "AKIA"),
            (HEADER_SECRET_ACCESS_KEY, "secret"),
            (HEADER_SESSION_TOKEN, "token"),
            (HEADER_REGION, "us-west-2"),
        ]);
        let creds = expect_static(parse_cred_headers(&h).unwrap());
        assert_eq!(creds.credentials.access_key_id(), "AKIA");
        assert_eq!(creds.credentials.secret_access_key(), "secret");
        assert_eq!(creds.credentials.session_token(), Some("token"));
        assert_eq!(creds.region.as_deref(), Some("us-west-2"));
    }

    #[test]
    fn long_lived_keys_without_token_or_region() {
        let h = headers(&[
            (HEADER_ACCESS_KEY_ID, "AKIA"),
            (HEADER_SECRET_ACCESS_KEY, "secret"),
        ]);
        let creds = expect_static(parse_cred_headers(&h).unwrap());
        assert_eq!(creds.credentials.session_token(), None);
        assert_eq!(creds.region, None);
    }

    #[test]
    fn empty_token_and_region_treated_as_absent() {
        let h = headers(&[
            (HEADER_ACCESS_KEY_ID, "AKIA"),
            (HEADER_SECRET_ACCESS_KEY, "secret"),
            (HEADER_SESSION_TOKEN, ""),
            (HEADER_REGION, ""),
        ]);
        let creds = expect_static(parse_cred_headers(&h).unwrap());
        assert_eq!(creds.credentials.session_token(), None);
        assert_eq!(creds.region, None);
    }

    #[test]
    fn akid_without_secret_is_incomplete() {
        let h = headers(&[(HEADER_ACCESS_KEY_ID, "AKIA")]);
        assert!(matches!(parse_cred_headers(&h), Err(CredError::Incomplete)));
    }

    #[test]
    fn secret_without_akid_is_incomplete() {
        let h = headers(&[(HEADER_SECRET_ACCESS_KEY, "secret")]);
        assert!(matches!(parse_cred_headers(&h), Err(CredError::Incomplete)));
    }

    #[test]
    fn valid_region_charset_accepted() {
        for region in [
            "us-east-1",
            "ap-southeast-2",
            "eu-central-1",
            "us-gov-west-1",
        ] {
            let h = headers(&[
                (HEADER_ACCESS_KEY_ID, "AKIA"),
                (HEADER_SECRET_ACCESS_KEY, "secret"),
                (HEADER_REGION, region),
            ]);
            let creds = expect_static(parse_cred_headers(&h).unwrap());
            assert_eq!(creds.region.as_deref(), Some(region));
        }
    }

    #[test]
    fn invalid_region_rejected() {
        // Uppercase, dots, slashes, spaces, and other host-significant ASCII
        // characters are rejected before reaching endpoint resolution.
        // (Non-ASCII bytes are caught even earlier as `Malformed` by `to_str`.)
        //
        // Also rejects degenerate shapes that pass a pure charset check but are
        // not valid region names: a bare/leading/trailing hyphen, consecutive
        // hyphens, and a leading digit.
        for region in [
            "US-EAST-1",
            "evil.com",
            "us-east-1/../foo",
            "us east 1",
            "us_east_1",
            "-",
            "-us-east-1",
            "us-east-",
            "us--east-1",
            "1-east-1",
        ] {
            let h = headers(&[
                (HEADER_ACCESS_KEY_ID, "AKIA"),
                (HEADER_SECRET_ACCESS_KEY, "secret"),
                (HEADER_REGION, region),
            ]);
            assert!(
                matches!(parse_cred_headers(&h), Err(CredError::InvalidRegion)),
                "expected {region:?} to be rejected"
            );
        }
    }

    #[test]
    fn overlong_region_rejected() {
        let h = headers(&[
            (HEADER_ACCESS_KEY_ID, "AKIA"),
            (HEADER_SECRET_ACCESS_KEY, "secret"),
            (HEADER_REGION, &"a".repeat(41)),
        ]);
        assert!(matches!(
            parse_cred_headers(&h),
            Err(CredError::InvalidRegion)
        ));
    }

    #[test]
    fn role_arn_parses_to_assume_role() {
        let h = headers(&[
            (
                HEADER_ROLE_ARN,
                "arn:aws:iam::123456789012:role/dial9-reader",
            ),
            (HEADER_REGION, "us-east-1"),
        ]);
        match parse_cred_headers(&h).unwrap() {
            CredSource::AssumeRole { role_arn, region } => {
                assert_eq!(
                    role_arn.as_str(),
                    "arn:aws:iam::123456789012:role/dial9-reader"
                );
                assert_eq!(region.as_deref(), Some("us-east-1"));
            }
            other => panic!("expected AssumeRole, got {other:?}"),
        }
    }

    #[test]
    fn role_arn_without_region_is_allowed() {
        let h = headers(&[(HEADER_ROLE_ARN, "arn:aws:iam::123456789012:role/r")]);
        match parse_cred_headers(&h).unwrap() {
            CredSource::AssumeRole { region, .. } => assert_eq!(region, None),
            other => panic!("expected AssumeRole, got {other:?}"),
        }
    }

    #[test]
    fn empty_role_arn_treated_as_absent() {
        // An empty role-arn header is absent, not a malformed ARN: falls through
        // to the default (no-credentials) source.
        let h = headers(&[(HEADER_ROLE_ARN, "")]);
        assert_eq!(parse_cred_headers(&h).unwrap(), CredSource::Default);
    }

    #[test]
    fn byoc_and_role_arn_together_conflict() {
        let h = headers(&[
            (HEADER_ACCESS_KEY_ID, "AKIA"),
            (HEADER_SECRET_ACCESS_KEY, "secret"),
            (HEADER_ROLE_ARN, "arn:aws:iam::123456789012:role/r"),
        ]);
        assert!(matches!(
            parse_cred_headers(&h),
            Err(CredError::ConflictingCredentials)
        ));
    }

    #[test]
    fn lone_akid_with_role_arn_still_conflicts() {
        // Even a half BYOC credential alongside a role ARN is the ambiguous
        // combination — conflict takes precedence over the incomplete check.
        let h = headers(&[
            (HEADER_ACCESS_KEY_ID, "AKIA"),
            (HEADER_ROLE_ARN, "arn:aws:iam::123456789012:role/r"),
        ]);
        assert!(matches!(
            parse_cred_headers(&h),
            Err(CredError::ConflictingCredentials)
        ));
    }

    #[test]
    fn valid_role_arns_accepted() {
        for arn in [
            "arn:aws:iam::123456789012:role/dial9-reader",
            "arn:aws:iam::123456789012:role/path/to/Reader_Role",
            "arn:aws-cn:iam::123456789012:role/r",
            "arn:aws-us-gov:iam::123456789012:role/r",
        ] {
            assert!(is_valid_role_arn(arn), "expected {arn:?} to be valid");
        }
    }

    #[test]
    fn invalid_role_arns_rejected() {
        for arn in [
            "",
            "not-an-arn",
            "arn:aws:iam::123456789012:user/bob", // not a role
            "arn:aws:s3:::bucket/key",            // wrong service
            "arn:aws:iam:us-east-1:123456789012:role/r", // region must be empty
            "arn:aws:iam::12345:role/r",          // account not 12 digits
            "arn:aws:iam::123456789012:role/",    // empty role name
            "arn:aws:iam::123456789012:role/*",   // wildcard
            "arn:evil:iam::123456789012:role/r",  // unknown partition
        ] {
            assert!(!is_valid_role_arn(arn), "expected {arn:?} to be rejected");
        }
    }

    #[test]
    fn overlong_role_arn_rejected() {
        // A role name past the 2048-char ARN cap is rejected before the SDK.
        let arn = format!("arn:aws:iam::123456789012:role/{}", "a".repeat(2048));
        assert!(!is_valid_role_arn(&arn));
    }

    // --- query-param (linkable) assume-role parsing ---

    #[test]
    fn role_arn_from_query_parses_to_assume_role() {
        let q = "aws_role_arn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fdial9-reader\
                 &aws_region=us-west-2&bucket=traces";
        match parse_cred_inputs(&HeaderMap::new(), Some(q)).unwrap() {
            CredSource::AssumeRole { role_arn, region } => {
                assert_eq!(
                    role_arn.as_str(),
                    "arn:aws:iam::123456789012:role/dial9-reader"
                );
                assert_eq!(region.as_deref(), Some("us-west-2"));
            }
            other => panic!("expected AssumeRole, got {other:?}"),
        }
    }

    #[test]
    fn unrelated_query_params_ignored() {
        // The data endpoints carry bucket/prefix/etc.; with no cred params the
        // source is Default.
        let q = "bucket=traces&prefix=dial9-traces&tz=UTC";
        assert_eq!(
            parse_cred_inputs(&HeaderMap::new(), Some(q)).unwrap(),
            CredSource::Default
        );
    }

    #[test]
    fn invalid_role_arn_from_query_rejected() {
        let q = "aws_role_arn=not-an-arn";
        assert!(matches!(
            parse_cred_inputs(&HeaderMap::new(), Some(q)),
            Err(CredError::InvalidRoleArn)
        ));
    }

    #[test]
    fn invalid_region_from_query_rejected() {
        let q = "aws_role_arn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fr&aws_region=US-EAST-1";
        assert!(matches!(
            parse_cred_inputs(&HeaderMap::new(), Some(q)),
            Err(CredError::InvalidRegion)
        ));
    }

    #[test]
    fn role_arn_in_both_header_and_query_conflicts() {
        // Two sources naming a role is ambiguous — refuse rather than pick one.
        let h = headers(&[(HEADER_ROLE_ARN, "arn:aws:iam::123456789012:role/h")]);
        let q = "aws_role_arn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fq";
        assert!(matches!(
            parse_cred_inputs(&h, Some(q)),
            Err(CredError::ConflictingCredentials)
        ));
    }

    #[test]
    fn byoc_headers_with_query_role_arn_conflict() {
        // Query role ARN is still mutually exclusive with header BYOC keys.
        let h = headers(&[
            (HEADER_ACCESS_KEY_ID, "AKIA"),
            (HEADER_SECRET_ACCESS_KEY, "secret"),
        ]);
        let q = "aws_role_arn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fr";
        assert!(matches!(
            parse_cred_inputs(&h, Some(q)),
            Err(CredError::ConflictingCredentials)
        ));
    }

    #[test]
    fn header_role_arn_still_works_with_other_query_params() {
        // A header role ARN coexists with non-credential query params (bucket).
        let h = headers(&[(HEADER_ROLE_ARN, "arn:aws:iam::123456789012:role/r")]);
        match parse_cred_inputs(&h, Some("bucket=traces")).unwrap() {
            CredSource::AssumeRole { role_arn, .. } => {
                assert_eq!(role_arn.as_str(), "arn:aws:iam::123456789012:role/r");
            }
            other => panic!("expected AssumeRole, got {other:?}"),
        }
    }

    #[test]
    fn query_region_alone_is_default() {
        // Region without a role ARN is meaningless — no credentialed source.
        assert_eq!(
            parse_cred_inputs(&HeaderMap::new(), Some("aws_region=us-east-1")).unwrap(),
            CredSource::Default
        );
    }

    #[test]
    fn header_region_wins_over_query_region() {
        let h = headers(&[
            (HEADER_ROLE_ARN, "arn:aws:iam::123456789012:role/r"),
            (HEADER_REGION, "eu-west-1"),
        ]);
        match parse_cred_inputs(&h, Some("aws_region=us-east-1")).unwrap() {
            CredSource::AssumeRole { region, .. } => {
                assert_eq!(region.as_deref(), Some("eu-west-1"));
            }
            other => panic!("expected AssumeRole, got {other:?}"),
        }
    }

    // --- StsRoleAssumer over the real AssumeRoleProvider (replayed STS) ---

    /// Drive the production [`StsRoleAssumer`] against a replayed STS response to
    /// prove the [`AssumeRoleProvider`] wiring (region, credential extraction)
    /// yields the assumed credentials — without a network call. The canned XML
    /// shape mirrors aws-config's own AssumeRoleProvider tests.
    #[tokio::test]
    async fn sts_role_assumer_mints_assumed_credentials() {
        use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
        use aws_smithy_types::body::SdkBody;

        let http_client = StaticReplayClient::new(vec![ReplayEvent::new(
            http::Request::new(SdkBody::from("assume-role request")),
            http::Response::builder()
                .status(200)
                .body(SdkBody::from(
                    "<AssumeRoleResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\">\
                       <AssumeRoleResult><Credentials>\
                         <AccessKeyId>ASIAASSUMED</AccessKeyId>\
                         <SecretAccessKey>assumed-secret</SecretAccessKey>\
                         <SessionToken>assumed-token</SessionToken>\
                         <Expiration>2030-01-01T00:00:00Z</Expiration>\
                       </Credentials></AssumeRoleResult>\
                     </AssumeRoleResponse>",
                ))
                .unwrap(),
        )]);

        // Base config: static base credentials + the replay HTTP client, so the
        // provider's STS call is served from the canned response above. A time
        // source and sleep impl are required — AssumeRoleProvider uses them for
        // its credential-caching layer (production gets them from load_defaults).
        let config = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .credentials_provider(aws_sdk_s3::config::SharedCredentialsProvider::new(
                Credentials::new("base", "base", None, None, "test"),
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .time_source(aws_smithy_async::time::SystemTimeSource::new())
            .sleep_impl(aws_smithy_async::rt::sleep::TokioSleep::new())
            .http_client(http_client)
            .build();

        let assumer = StsRoleAssumer::from_config(config);
        let arn = RoleArn("arn:aws:iam::123456789012:role/dial9-reader".to_string());
        let temp = assumer
            .assume_role(&arn, Some("us-west-2"))
            .await
            .expect("assume-role should succeed against the replayed response");

        assert_eq!(temp.credentials.access_key_id(), "ASIAASSUMED");
        assert_eq!(temp.credentials.secret_access_key(), "assumed-secret");
        assert_eq!(temp.credentials.session_token(), Some("assumed-token"));
        // Region is forwarded from the request, not the base config.
        assert_eq!(temp.region.as_deref(), Some("us-west-2"));
    }
}
