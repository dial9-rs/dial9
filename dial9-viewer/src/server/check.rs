//! `POST /api/credentials/check` — validate bring-your-own credentials and
//! auto-detect the target bucket's region in a single round-trip, so the UI can
//! confirm credentials on "Apply" and then send a resolved region on every
//! later request.
//!
//! This is `POST`, not `GET`, because it is an action (it triggers a
//! side-effecting `HeadBucket` network call to validate credentials) rather
//! than a cacheable resource read. `POST` keeps the validation result from
//! being cached by browsers or intermediaries — important when the answer
//! depends on per-request credential headers.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use crate::server::credentials::{CredError, MaybeCreds};
use crate::storage::build_credentialed_client;

#[derive(Deserialize)]
pub struct CheckParams {
    pub bucket: Option<String>,
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

    let temp = match creds.0 {
        Ok(Some(temp)) => temp,
        Ok(None) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "credentials required: supply x-dial9-aws-* headers".to_string(),
            ));
        }
        Err(e @ (CredError::Incomplete | CredError::Malformed | CredError::InvalidRegion)) => {
            return Err((StatusCode::BAD_REQUEST, e.message().to_string()));
        }
    };

    let bucket = match params.bucket.or_else(|| state.default_bucket.clone()) {
        Some(b) => b,
        None => {
            return Err((StatusCode::BAD_REQUEST, "bucket is required".to_string()));
        }
    };

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
            // `is_valid_region` in `parse_cred_headers` (an invalid region is
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

/// Map a HeadBucket failure to a short, non-sensitive reason string.
fn classify_check_failure(
    err: &aws_sdk_s3::error::SdkError<
        aws_sdk_s3::operation::head_bucket::HeadBucketError,
        aws_sdk_s3::config::http::HttpResponse,
    >,
) -> String {
    use aws_sdk_s3::error::ProvideErrorMetadata;
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
