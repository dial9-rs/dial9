//! `POST /api/credentials/check` — validate bring-your-own credentials and
//! auto-detect the target bucket's region in a single round-trip, so the UI can
//! confirm credentials on "Apply" and then send a resolved region on every
//! later request.
//!
//! Without a prefix from the request or the server's default, the check is a
//! `HeadBucket`. With one, it is a `ListObjectsV2` limited to one key under
//! that prefix, so credentials scoped to part of a bucket can be validated
//! without bucket-level access.
//!
//! This is `POST`, not `GET`, because it is an action (it triggers a
//! side-effecting S3 network call to validate credentials) rather than a
//! cacheable resource read. `POST` keeps the validation result from being
//! cached by browsers or intermediaries — important when the answer depends on
//! per-request credential headers.

use aws_sdk_s3::config::http::HttpResponse;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use crate::server::browse::resolve_base;
use crate::server::credentials::{self, CredError, CredSource, MaybeCreds, TempCredentials};
use crate::storage::{DEFAULT_REGION, build_credentialed_client};

pub(crate) const CREDENTIALS_REQUIRED: &str =
    "credentials required: supply x-dial9-aws-* headers or a role ARN";

#[derive(Deserialize)]
pub struct CheckParams {
    pub bucket: Option<String>,
    /// Optional key prefix to validate instead of the whole bucket. Combined
    /// with the server's default prefix the same way `/api/browse` does.
    pub prefix: Option<String>,
}

#[derive(Serialize)]
pub struct CheckResponse {
    /// Whether the credentials could access the bucket.
    pub ok: bool,
    /// The bucket's region, if it could be detected.
    pub region: Option<String>,
    /// A short, non-sensitive error description when `ok` is false.
    pub error: Option<String>,
}

pub async fn check_credentials(
    State(state): State<AppState>,
    creds: MaybeCreds,
    Query(params): Query<CheckParams>,
) -> Result<Json<CheckResponse>, (StatusCode, String)> {
    if !state.allow_byo_creds {
        return Err((
            StatusCode::BAD_REQUEST,
            "this server does not accept user credentials".to_string(),
        ));
    }

    // Resolve the request's named identity to concrete credentials: BYOC keys
    // are used directly; a role ARN is assumed first (via the shared
    // `AppState::assume`, so this validates the same identity — and applies the
    // same error policy — as a later data request would).
    let temp = match creds.0 {
        Ok(CredSource::Static(temp)) => temp,
        Ok(CredSource::AssumeRole { role_arn, region }) => {
            state.assume(&role_arn, region.as_deref()).await?
        }
        Ok(CredSource::Default) => {
            return Err((StatusCode::BAD_REQUEST, CREDENTIALS_REQUIRED.to_string()));
        }
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

    let bucket = match params.bucket.or_else(|| state.default_bucket.clone()) {
        Some(b) => b,
        None => {
            return Err((StatusCode::BAD_REQUEST, "bucket is required".to_string()));
        }
    };

    if let Some(prefix) = scoped_prefix(state.default_prefix.as_deref(), params.prefix.as_deref()) {
        return Ok(Json(check_prefix(&state, temp, &bucket, &prefix).await));
    }

    let client = build_credentialed_client(
        temp.credentials,
        temp.region.as_deref(),
        &state.ephemeral_s3,
    );

    // HeadBucket both validates the credentials and reveals the bucket region
    // (directly on success, or via the x-amz-bucket-region header on the
    // region-mismatch redirect).
    match client.head_bucket().bucket(&bucket).send().await {
        Ok(resp) => Ok(Json(CheckResponse {
            ok: true,
            // Prefer S3's reported bucket region; fall back to the
            // caller-supplied region. The fallback is safe: `temp` came from
            // `MaybeCreds`, whose region was already run through
            // `is_valid_region` in `parse_cred_inputs` (an invalid region is
            // rejected as `CredError::InvalidRegion` before reaching here), so
            // it is either `None` or a syntactically valid region name.
            region: resp.bucket_region().map(|r| r.to_string()).or(temp.region),
            error: None,
        })),
        Err(err) => {
            // A region-mismatch redirect (HTTP 301) carries the bucket region
            // and means the credentials are VALID — just aimed at the wrong
            // regional endpoint. Treat ONLY that as success.
            //
            // Important: S3 also returns `x-amz-bucket-region` on a 403
            // auth-failure response, so we must gate on the 301 status — keying
            // off the header alone would mis-report bad credentials as valid.
            let raw = err.raw_response();
            let status = raw.map(|r| r.status().as_u16());
            let redirect_region = raw.and_then(|r| {
                r.headers()
                    .get("x-amz-bucket-region")
                    .map(|v| v.to_string())
            });
            if let (Some(301), Some(region)) = (status, redirect_region) {
                return Ok(Json(CheckResponse {
                    ok: true,
                    region: Some(region),
                    error: None,
                }));
            }
            // Otherwise the credentials (or bucket access) were rejected. Return
            // a generic reason — never echo the SDK message, which can contain
            // the access key id.
            Ok(Json(CheckResponse {
                ok: false,
                region: None,
                error: Some(classify_check_failure(&err)),
            }))
        }
    }
}

/// The `ListObjectsV2` prefix for a scoped check, or `None` for a bucket-level
/// check. The request prefix is combined with the server default exactly as
/// `/api/browse` does, so a server default alone still scopes the check. The
/// result gets a single trailing `/` so the listing stays inside that
/// directory, matching the keys browse lists under it.
fn scoped_prefix(default_prefix: Option<&str>, requested: Option<&str>) -> Option<String> {
    let requested = requested.map(str::trim).filter(|p| !p.is_empty());
    let base = resolve_base(default_prefix, requested);
    let base = base.trim_matches('/');
    (!base.is_empty()).then(|| format!("{base}/"))
}

/// Validate `temp` by listing at most one key under `prefix`.
///
/// A wrong-region redirect says where the bucket lives but nothing about
/// whether these credentials may read the prefix, so the listing is repeated
/// once against the bucket's region and only that answer decides the result.
async fn check_prefix(
    state: &AppState,
    temp: TempCredentials,
    bucket: &str,
    prefix: &str,
) -> CheckResponse {
    let client = build_credentialed_client(
        temp.credentials.clone(),
        temp.region.as_deref(),
        &state.ephemeral_s3,
    );
    let err = match list_one_key(&client, bucket, prefix).await {
        Ok(()) => return CheckResponse::accessible(temp.region),
        Err(err) => err,
    };
    // The region the first attempt was signed for.
    let client_region = temp.region.as_deref().unwrap_or(DEFAULT_REGION);
    let Some(region) = redirect_region(&*err).filter(|r| r != client_region) else {
        return CheckResponse::rejected(&*err);
    };
    tracing::debug!(%region, "retrying scoped credential check in the bucket's region");
    let client = build_credentialed_client(temp.credentials, Some(&region), &state.ephemeral_s3);
    match list_one_key(&client, bucket, prefix).await {
        Ok(()) => CheckResponse::accessible(Some(region)),
        Err(err) => CheckResponse::rejected(&*err),
    }
}

type ListObjectsV2SdkError =
    SdkError<aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Error, HttpResponse>;

async fn list_one_key(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
) -> Result<(), Box<ListObjectsV2SdkError>> {
    client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .max_keys(1)
        .send()
        .await
        .map(|_| ())
        .map_err(Box::new)
}

/// The bucket region S3 names on a wrong-region response. S3 also sends
/// `x-amz-bucket-region` on access-denied responses, so only redirect-shaped
/// errors count, and the value must be a region name before it can select an
/// endpoint.
fn redirect_region<E: ProvideErrorMetadata>(err: &SdkError<E, HttpResponse>) -> Option<String> {
    let raw = err.raw_response()?;
    let redirect = raw.status().as_u16() == 301
        || matches!(
            err.code(),
            Some(
                "PermanentRedirect"
                    | "Redirect"
                    | "AuthorizationHeaderMalformed"
                    | "IllegalLocationConstraintException"
            )
        );
    if !redirect {
        return None;
    }
    raw.headers()
        .get("x-amz-bucket-region")
        .filter(|region| credentials::is_valid_region(region))
        .map(str::to_string)
}

impl CheckResponse {
    fn accessible(region: Option<String>) -> Self {
        Self {
            ok: true,
            region,
            error: None,
        }
    }

    /// Never echo the SDK message, which can contain the access key id.
    fn rejected<E: ProvideErrorMetadata>(err: &SdkError<E, HttpResponse>) -> Self {
        Self {
            ok: false,
            region: None,
            error: Some(classify_check_failure(err)),
        }
    }
}

/// Map an S3 check failure to a short, non-sensitive reason string.
fn classify_check_failure<E: ProvideErrorMetadata>(err: &SdkError<E, HttpResponse>) -> String {
    match err.code() {
        Some("InvalidAccessKeyId" | "UnrecognizedClientException" | "InvalidClientTokenId") => {
            return "access key id not recognized".to_string();
        }
        Some("SignatureDoesNotMatch") => return "secret access key is incorrect".to_string(),
        Some("ExpiredToken" | "ExpiredTokenException" | "InvalidToken") => {
            return "session token is invalid or expired".to_string();
        }
        Some("AccessDenied" | "AccessDeniedException" | "Forbidden") => {
            return "access denied for this bucket".to_string();
        }
        Some("NoSuchBucket" | "NotFound") => return "bucket not found".to_string(),
        _ => {}
    }
    // HeadBucket is a HEAD request, so error responses have no body and the SDK
    // can't parse an error code. Fall back to the HTTP status, which still
    // distinguishes the common cases.
    match err.raw_response().map(|r| r.status().as_u16()) {
        Some(401 | 403) => {
            "credentials rejected or access denied (check keys, token, or permissions)".to_string()
        }
        Some(404) => "bucket not found".to_string(),
        _ => "could not access bucket with these credentials".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{EphemeralS3Config, LocalBackend, StorageBackend};
    use aws_smithy_http_client::test_util::infallible_client_fn;
    use std::sync::{Arc, Mutex};

    const LIST_OK: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
        <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
          <Name>traces-bucket</Name><KeyCount>0</KeyCount><MaxKeys>1</MaxKeys>
          <IsTruncated>false</IsTruncated>
        </ListBucketResult>"#;

    fn error_body(code: &str) -> String {
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code></Error>")
    }

    /// A canned S3 reply: status, optional `x-amz-bucket-region`, body.
    type Reply = (u16, Option<&'static str>, String);

    /// What the check sent: method, path and query, signing region.
    #[derive(Debug)]
    struct Sent {
        method: String,
        uri: String,
        signing_region: String,
    }

    /// Run the check, without a caller-supplied region, against an S3 fake that
    /// replays `replies` in order and records every request it receives.
    async fn run_check(
        default_prefix: Option<&str>,
        prefix: Option<&str>,
        replies: Vec<Reply>,
    ) -> (CheckResponse, Vec<Sent>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(replies.into_iter()));
        let http_client = infallible_client_fn({
            let sent = Arc::clone(&sent);
            move |req: http::Request<_>| {
                let auth = req.headers()["authorization"].to_str().unwrap();
                let signing_region = auth.split('/').nth(2).unwrap().to_string();
                sent.lock().unwrap().push(Sent {
                    method: req.method().to_string(),
                    uri: req.uri().to_string(),
                    signing_region,
                });
                let (status, bucket_region, body) = replies
                    .lock()
                    .unwrap()
                    .next()
                    .expect("unexpected S3 request");
                let mut resp = http::Response::builder().status(status);
                if let Some(bucket_region) = bucket_region {
                    resp = resp.header("x-amz-bucket-region", bucket_region);
                }
                resp.body(body).unwrap()
            }
        });
        let backend: Arc<dyn StorageBackend> = Arc::new(LocalBackend::new_temporary_aggregate());
        let state = AppState::new(backend, None, default_prefix.map(str::to_string))
            .with_byo_creds(true)
            .with_ephemeral_s3(EphemeralS3Config {
                http_client,
                endpoint_url: Some("http://s3.test".to_string()),
                force_path_style: true,
            });
        let creds = MaybeCreds(Ok(CredSource::Static(TempCredentials::new(
            "AKIDEXAMPLE",
            "secret",
            None,
            None,
        ))));
        // Parsed from a URI so the test also pins the `prefix` query name.
        let uri = match prefix {
            Some(prefix) => format!("/?bucket=traces-bucket&prefix={prefix}"),
            None => "/?bucket=traces-bucket".to_string(),
        };
        let params = Query::try_from_uri(&uri.parse().unwrap()).unwrap();
        let Json(resp) = check_credentials(State(state), creds, params)
            .await
            .expect("check handler failed");
        let sent = std::mem::take(&mut *sent.lock().unwrap());
        (resp, sent)
    }

    /// Assert `sent` is a `ListObjectsV2` for one key under `prefix`, signed
    /// for `region`.
    fn assert_scoped_list(sent: &Sent, prefix: &str, region: &str) {
        let query: Vec<(String, String)> =
            form_urlencoded::parse(sent.uri.split_once('?').unwrap().1.as_bytes())
                .into_owned()
                .collect();
        for param in [("list-type", "2"), ("max-keys", "1"), ("prefix", prefix)] {
            let param = (param.0.to_string(), param.1.to_string());
            assert!(query.contains(&param), "missing {param:?} in {sent:?}");
        }
        assert_eq!(sent.signing_region, region);
    }

    #[rstest::rstest]
    #[case::trims_space_and_slashes(None, Some(" /traces/svc/ "), Some("traces/svc/"))]
    #[case::nests_under_server_default(Some("traces"), Some("svc"), Some("traces/svc/"))]
    #[case::already_under_server_default(Some("traces"), Some("traces/svc"), Some("traces/svc/"))]
    #[case::server_default_alone(Some("/traces/"), Some(" "), Some("traces/"))]
    #[case::absent(None, None, None)]
    #[case::only_slashes(Some("/"), Some("/"), None)]
    fn scoped_prefix_normalizes_and_composes(
        #[case] default_prefix: Option<&str>,
        #[case] requested: Option<&str>,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(
            scoped_prefix(default_prefix, requested).as_deref(),
            expected
        );
    }

    /// A scoped check lists at most one key under the combined prefix and
    /// accepts an empty listing.
    #[tokio::test]
    async fn scoped_check_lists_one_key_under_the_prefix() {
        let (resp, sent) = run_check(
            Some("traces"),
            Some("svc"),
            vec![(200, None, LIST_OK.to_string())],
        )
        .await;
        assert!(resp.ok, "{:?}", resp.error);
        assert_eq!(sent.len(), 1);
        assert_scoped_list(&sent[0], "traces/svc/", DEFAULT_REGION);
    }

    #[tokio::test]
    async fn unscoped_check_uses_head_bucket() {
        let (resp, sent) = run_check(None, None, vec![(200, None, String::new())]).await;
        assert!(resp.ok, "{:?}", resp.error);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method, "HEAD", "{:?}", sent[0]);
    }

    /// A wrong-region response, by status or by error code, is retried in the
    /// reported region, and only the retry's answer decides the result.
    #[rstest::rstest]
    #[case::redirect_then_allowed(
        (301, Some("us-west-2"), error_body("PermanentRedirect")),
        (200, None, LIST_OK.to_string()),
        true
    )]
    #[case::malformed_auth_then_allowed(
        (400, Some("us-west-2"), error_body("AuthorizationHeaderMalformed")),
        (200, None, LIST_OK.to_string()),
        true
    )]
    #[case::redirect_then_denied(
        (301, Some("us-west-2"), error_body("PermanentRedirect")),
        (403, Some("us-west-2"), error_body("AccessDenied")),
        false
    )]
    #[tokio::test]
    async fn scoped_check_retries_in_the_reported_region(
        #[case] first: Reply,
        #[case] retry: Reply,
        #[case] ok: bool,
    ) {
        let (resp, sent) = run_check(None, Some("traces"), vec![first, retry]).await;
        assert_eq!(resp.ok, ok, "{:?}", resp.error);
        assert_eq!(resp.region.as_deref(), ok.then_some("us-west-2"));
        assert_eq!(sent.len(), 2);
        assert_scoped_list(&sent[1], "traces/", "us-west-2");
    }

    /// S3 sends `x-amz-bucket-region` on access-denied responses too; that is
    /// a final rejection, as is a redirect whose region is not a region name.
    #[rstest::rstest]
    #[case::denied_with_region_header(
        (403, Some("us-west-2"), error_body("AccessDenied")),
        "access denied for this bucket"
    )]
    #[case::invalid_region_header(
        (301, Some("us-west-2.example.com"), error_body("PermanentRedirect")),
        "could not access bucket with these credentials"
    )]
    #[tokio::test]
    async fn scoped_check_rejection_is_not_retried(#[case] reply: Reply, #[case] error: &str) {
        let (resp, sent) = run_check(None, Some("traces"), vec![reply]).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some(error));
        assert_eq!(sent.len(), 1);
    }
}
