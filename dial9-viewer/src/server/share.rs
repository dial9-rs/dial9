//! Shareable link endpoints.
//!
//! `POST /api/shared` — upload the current trace buffer to the server's
//! configured S3 bucket under `shared/<token>.bin.gz`, returning a shareable
//! link whose security relies on the token's unguessability (128-bit UUID v4).
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
    validate_trace_body(&body)?;

    let bucket = state.default_bucket.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "sharing not available: no bucket configured".to_string(),
    ))?;

    let token = uuid::Uuid::new_v4().simple().to_string();
    let key = format!("shared/{token}.bin.gz");

    // Gzip the body for storage efficiency.
    let compressed = gzip_bytes(&body).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("compression failed: {e}"),
        )
    })?;

    state
        .backend
        .put_object(bucket, &key, compressed, Some("application/gzip"))
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
        .unwrap()
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
