//! Shared mapping from [`StorageError`] to HTTP responses.

use crate::storage::StorageError;
use axum::http::StatusCode;

/// Convert a [`StorageError`] into an HTTP `(status, body)` pair.
///
/// Authorization failures collapse to a generic `401` so the underlying SDK
/// message — which can echo the access key id — never reaches the client.
pub fn storage_error_response(err: StorageError) -> (StatusCode, String) {
    match err {
        StorageError::Unauthorized => (StatusCode::UNAUTHORIZED, err.to_string()),
        StorageError::AccountNotSignedUp => (StatusCode::FORBIDDEN, err.to_string()),
        StorageError::NotFound(_) => (StatusCode::NOT_FOUND, err.to_string()),
        // 421 Misdirected Request: the request reached an endpoint (region) that
        // cannot serve this bucket — semantically exact for an S3 region
        // mismatch, and distinct from a generic 500 so the UI can surface the
        // actionable "set the region" message.
        StorageError::WrongRegion => (StatusCode::MISDIRECTED_REQUEST, err.to_string()),
        StorageError::Other(_) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}
