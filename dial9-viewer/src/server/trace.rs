use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Query;
use futures::TryStreamExt;
use serde::Deserialize;

use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::error::storage_error_response;

#[derive(Deserialize)]
pub struct ObjectParams {
    /// A single S3 key (e.g. ?key=2026-04-09/.../123-0.bin.gz)
    pub key: String,
    pub bucket: Option<String>,
}

/// `GET /api/object?bucket=&key=` — stream a single object's bytes verbatim.
///
/// Unlike [`get_trace`], this does NOT decompress: a `.bin.gz` object is served
/// still-gzipped. The viewer fetches one `trace=/api/object?…` component per
/// file in parallel and gunzips each client-side (see `fetchTraces` in
/// `trace_parser.js`). Keeping the bytes compressed on the wire is the whole
/// point — far less network transfer than the old server-side-merged response.
///
/// The body is streamed straight from the backend (see
/// [`StorageBackend::get_object_stream`]) rather than buffered: bytes reach the
/// browser as S3 delivers them, removing the ~2s time-to-first-byte stall the
/// old `collect()`-then-send path imposed.
///
/// IMPORTANT: we deliberately do NOT set `content-encoding: gzip` even though
/// the object is gzip-compressed. We serve the raw gzip bytes opaquely and the
/// browser gunzips them itself via `DecompressionStream` in `fetchTraceStream`.
/// Setting `content-encoding: gzip` would make the browser transparently
/// decompress the body, and the client-side decoder would then double-handle
/// (or fail on) already-decompressed bytes.
pub async fn get_object(
    State(state): State<AppState>,
    creds: MaybeCreds,
    Query(params): Query<ObjectParams>,
) -> Result<Response, (StatusCode, String)> {
    let backend = state.resolve(creds).await?;

    let bucket = params
        .bucket
        .or(state.default_bucket.clone())
        .ok_or((StatusCode::BAD_REQUEST, "bucket is required".to_string()))?;

    let key = params.key;
    if key.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "key is required".to_string()));
    }

    // Setup errors (not found / auth) surface here, before any body streams, so
    // they still map to the right status. Mid-stream errors (below) cannot.
    let object = backend
        .get_object_stream(&bucket, &key)
        .await
        .map_err(storage_error_response)?;

    // Stream-scoped metric: emitted when the body stream is dropped (normal end
    // or truncation). Moved into the `inspect_err` closure below so it lives as
    // long as the stream; the closure flips `truncated_mid_stream` on error.
    // This is a *separate* entry from the per-request metric because the outcome
    // is only known after the 200 headers are already sent.
    let mut stream_metrics = crate::server::metrics::ObjectStreamMetrics::arm("/api/object");

    // Log a chunk error rather than dropping it: once streaming has begun the
    // status line is already sent, so this is the only signal that the response
    // was truncated. Per-request (not in a loop), so a plain warn! is fine.
    let body_stream = object.stream.inspect_err(move |e| {
        stream_metrics.truncated_mid_stream = 1;
        tracing::warn!(
            bucket = %bucket,
            key = %key,
            error = %e,
            "error mid-stream while serving /api/object; response is truncated"
        );
    });

    let mut builder = Response::builder().header("content-type", "application/octet-stream");
    if let Some(len) = object.content_length {
        builder = builder.header("content-length", len);
    }

    Ok(builder
        .body(Body::from_stream(body_stream))
        .unwrap()
        .into_response())
}
