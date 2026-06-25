//! Shareable link endpoints.
//!
//! `POST /api/shared` — upload the current trace buffer to the server's
//! configured S3 bucket under `shared/<token>.bin.gz`, returning a shareable
//! link whose security relies on the token's unguessability (UUID v4, 122 bits
//! of entropy).
//!
//! `GET /api/shared/{token}` — retrieve a previously shared trace. No
//! credentials are required from the requester; the server fetches the object
//! using its own ambient identity.

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Serialize;
use std::io::Write;

use crate::server::AppState;
use crate::server::error::storage_error_response;
use crate::server::trace::maybe_gunzip;
use crate::server::upload::validate_trace_body;

/// Hard cap on the size of a single shared trace, in bytes (256 MiB).
///
/// Shared traces are written to S3 with no eviction, count quota, or TTL, so an
/// unbounded `put_object` would let a caller fill the bucket. This bounds a
/// single share; it is enforced both as the route's `DefaultBodyLimit` (rejects
/// before buffering the whole body) and in the handler (source of truth, so the
/// cap holds regardless of how the route is wired).
pub(super) const MAX_SHARED_TRACE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Serialize)]
struct ShareResponse {
    token: String,
    trace_url: String,
    viewer_url: String,
}

/// `POST /api/shared` — store the request body as a shared trace in S3.
pub async fn create_shared(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    if !state.sharing_enabled {
        return Err((StatusCode::NOT_FOUND, "sharing not enabled".to_string()));
    }

    validate_trace_body(&body)?;

    // Hard size cap. The route's body limit also enforces this (and rejects
    // before fully buffering), but check here too so the handler that owns the
    // limit is the source of truth.
    let max_bytes = state.max_shared_trace_bytes;
    if body.len() > max_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("trace exceeds the {max_bytes}-byte share limit"),
        ));
    }

    let bucket = state.default_bucket.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "sharing not available: no bucket configured".to_string(),
    ))?;

    let token = uuid::Uuid::new_v4().simple().to_string();
    let key = format!("shared/{token}.bin.gz");

    // Store gzipped. If the body is already gzipped, store verbatim to avoid
    // double-compression; otherwise gzip it for storage efficiency.
    let stored = if body.starts_with(&super::upload::GZIP_MAGIC) {
        body.to_vec()
    } else {
        gzip_bytes(&body).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("compression failed: {e}"),
            )
        })?
    };

    state
        .backend
        .put_object(bucket, &key, stored, Some("application/gzip"))
        .await
        .map_err(storage_error_response)?;

    tracing::info!(%token, bytes = body.len(), "created shared trace");

    let resp = ShareResponse {
        trace_url: format!("/api/shared/{token}"),
        viewer_url: format!("/viewer.html?shared={token}"),
        token,
    };
    Ok((StatusCode::OK, axum::Json(resp)).into_response())
}

/// `GET /api/shared/{token}` — serve a previously shared trace.
pub async fn get_shared(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    if !state.sharing_enabled {
        return Err((StatusCode::NOT_FOUND, "sharing not enabled".to_string()));
    }

    if !is_valid_token(&token) {
        return Err((StatusCode::BAD_REQUEST, "invalid token".to_string()));
    }

    let bucket = state.default_bucket.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "sharing not available: no bucket configured".to_string(),
    ))?;

    let key = format!("shared/{token}.bin.gz");
    let data = state
        .backend
        .get_object(bucket, &key)
        .await
        .map_err(storage_error_response)?;

    let raw = maybe_gunzip(&data);

    Ok(Response::builder()
        .header("content-type", "application/octet-stream")
        .header("content-disposition", "attachment; filename=\"trace.bin\"")
        .body(Body::from(raw))
        .expect("static headers and owned body are always valid")
        .into_response())
}

/// A valid token is exactly 32 lowercase hex characters (UUID v4 simple form).
fn is_valid_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn gzip_bytes(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data)?;
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_tokens() {
        assert!(is_valid_token("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6"));
        assert!(is_valid_token("00000000000000000000000000000000"));
    }

    #[test]
    fn invalid_tokens() {
        assert!(!is_valid_token(""));
        assert!(!is_valid_token("short"));
        assert!(!is_valid_token("A1B2C3D4E5F6A7B8C9D0E1F2A3B4C5D6")); // uppercase
        assert!(!is_valid_token("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6x")); // too long
        assert!(!is_valid_token("../shared/a1b2c3d4e5f6a7b8c9d0e1f2")); // path traversal
    }
}
