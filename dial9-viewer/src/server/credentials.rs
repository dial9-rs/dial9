//! Bring-your-own-credentials: per-request AWS credentials supplied by the
//! browser as `x-dial9-aws-*` headers, so the backend can call S3 on the
//! user's behalf without holding any standing access.
//!
//! Transport is per-request headers (no server-side session store). The
//! [`MaybeCreds`] extractor parses the headers; [`crate::server::AppState::resolve`]
//! turns them into an ephemeral [`crate::storage::S3Backend`].

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

/// Provider name attached to the SDK [`Credentials`] we build. Surfaces in SDK
/// diagnostics so BYO credentials are distinguishable from ambient ones.
const PROVIDER_NAME: &str = "dial9-byo";

/// User-supplied temporary credentials plus an optional region.
///
/// The credentials are stored in the AWS SDK's own [`Credentials`] type rather
/// than loose fields: it is exactly what we hand to `.credentials_provider(..)`,
/// it natively carries an optional expiry for temporary credentials, and there
/// is no second representation to keep in sync.
#[derive(Clone)]
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

/// Why credential headers could not be turned into [`TempCredentials`].
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
}

impl CredError {
    pub fn message(&self) -> &'static str {
        match self {
            CredError::Incomplete => {
                "incomplete credentials: both access key id and secret access key are required"
            }
            CredError::Malformed => "malformed credential header",
            CredError::InvalidRegion => "invalid region",
        }
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

/// Infallible extractor over the `x-dial9-aws-*` headers.
///
/// - all credential headers absent  → `Ok(None)` (use the server's default backend)
/// - access-key-id + secret present → `Ok(Some(..))`
/// - exactly one of the two present → `Err(CredError::Incomplete)`
///
/// The extractor is deliberately pure header-parsing and state-agnostic; the
/// decision of *what backend to build* lives in `AppState::resolve` where the
/// server config is available.
pub struct MaybeCreds(pub Result<Option<TempCredentials>, CredError>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeCreds {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        Ok(MaybeCreds(parse_cred_headers(&parts.headers)))
    }
}

/// Pull credentials out of a header map. Split from the extractor so it can be
/// unit-tested without constructing a request.
pub fn parse_cred_headers(
    headers: &axum::http::HeaderMap,
) -> Result<Option<TempCredentials>, CredError> {
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
    // An empty region header is treated as absent; a non-empty one must be a
    // valid AWS region name (it ends up in the S3 endpoint host).
    let region = match get(HEADER_REGION)?.filter(|s| !s.is_empty()) {
        Some(r) if !is_valid_region(&r) => return Err(CredError::InvalidRegion),
        other => other,
    };

    match (access_key_id, secret_access_key) {
        (None, None) => Ok(None),
        (Some(akid), Some(secret)) => Ok(Some(TempCredentials::new(
            akid,
            secret,
            session_token.filter(|s| !s.is_empty()),
            region,
        ))),
        // Exactly one half present — refuse rather than silently fall back.
        _ => Err(CredError::Incomplete),
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

    #[test]
    fn absent_headers_yield_none() {
        let parsed = parse_cred_headers(&HeaderMap::new()).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn full_credentials_parse() {
        let h = headers(&[
            (HEADER_ACCESS_KEY_ID, "AKIA"),
            (HEADER_SECRET_ACCESS_KEY, "secret"),
            (HEADER_SESSION_TOKEN, "token"),
            (HEADER_REGION, "us-west-2"),
        ]);
        let creds = parse_cred_headers(&h).unwrap().unwrap();
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
        let creds = parse_cred_headers(&h).unwrap().unwrap();
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
        let creds = parse_cred_headers(&h).unwrap().unwrap();
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
            let creds = parse_cred_headers(&h).unwrap().unwrap();
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
}
