use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Query;
use flate2::read::GzDecoder;
use futures::TryStreamExt;
use futures::future::join_all;
use serde::Deserialize;
use std::io::Read;

use crate::server::AppState;
use crate::server::credentials::MaybeCreds;
use crate::server::error::storage_error_response;

const MAX_KEYS: usize = 100;

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
    let backend = state.resolve(creds)?;

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

    // Log a chunk error rather than dropping it: once streaming has begun the
    // status line is already sent, so this is the only signal that the response
    // was truncated. Per-request (not in a loop), so a plain warn! is fine.
    let body_stream = object.stream.inspect_err(move |e| {
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

#[derive(Deserialize)]
pub struct TraceParams {
    /// S3 keys (repeated query param: ?keys=a&keys=b)
    #[serde(default)]
    pub keys: Vec<String>,
    pub bucket: Option<String>,
}

/// `GET /api/trace?bucket=&keys=a&keys=b` — fetch every key, gunzip each, and
/// concatenate into one uncompressed response.
///
/// DEPRECATED: scheduled for removal. The viewer no longer links here; it now
/// emits one `trace=/api/object?…` component per file and lets the browser
/// download them in parallel and gunzip client-side ([`get_object`]). This
/// endpoint forces the backend to decompress and buffer the whole merged trace,
/// which transfers far more bytes. It remains only for out-of-tree callers (the
/// `dial9-trace-loading` skill); new code should use `/api/object`.
pub async fn get_trace(
    State(state): State<AppState>,
    creds: MaybeCreds,
    Query(params): Query<TraceParams>,
) -> Result<Response, (StatusCode, String)> {
    let backend = state.resolve(creds)?;

    let bucket = params
        .bucket
        .or(state.default_bucket.clone())
        .ok_or((StatusCode::BAD_REQUEST, "bucket is required".to_string()))?;

    let keys: Vec<&str> = params
        .keys
        .iter()
        .map(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .collect();
    if keys.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "keys is required".to_string()));
    }
    if keys.len() > MAX_KEYS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("too many keys (max {MAX_KEYS})"),
        ));
    }

    let fetches = keys.iter().map(|key| backend.get_object(&bucket, key));
    let results = join_all(fetches).await;

    let mut combined = Vec::new();
    for result in results {
        let data = result.map_err(storage_error_response)?;
        let raw = maybe_gunzip(&data);
        combined.extend_from_slice(&raw);
    }

    Ok(Response::builder()
        .header("content-type", "application/octet-stream")
        .header("content-disposition", "attachment; filename=\"trace.bin\"")
        .body(Body::from(combined))
        .unwrap()
        .into_response())
}

fn maybe_gunzip(data: &[u8]) -> Vec<u8> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        let mut decoder = GzDecoder::new(data);
        let mut decompressed = Vec::new();
        match decoder.read_to_end(&mut decompressed) {
            Ok(_) => decompressed,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "gzip header detected but decompression failed, returning raw bytes"
                );
                data.to_vec()
            }
        }
    } else {
        data.to_vec()
    }
}
