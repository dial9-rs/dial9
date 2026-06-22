//! Temporary in-memory trace uploads.
//!
//! Lets another site `POST` a trace file to the viewer and get back a link its
//! users can open. Uploads live only in memory: they are bounded by per-upload
//! size, total-bytes, and count caps, and are reclaimed by a TTL. Each upload is
//! **single-use** — the first successful `GET` removes it, so a reload or a
//! second tab will 404. There is no auth; safety relies on the caps plus where
//! the viewer is deployed on the network.
//!
//! The bytes are stored and served verbatim (gzipped or raw). The viewer's
//! fetch path already sniffs and gunzips client-side, exactly as it does for any
//! other `?trace=<url>`, so no server-side decoding happens here.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::server::AppState;

/// gzip magic (RFC 1952): a gzipped trace starts with these two bytes.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// Trace-format magic `TRC\0` (see `dial9-trace-format` `codec::MAGIC`). A raw,
/// uncompressed trace starts with these four bytes.
const TRACE_MAGIC: [u8; 4] = [0x54, 0x52, 0x43, 0x00];

/// Caps on the in-memory upload store. Defaults are sized for a viewer host with
/// a few GiB of headroom; tests construct small limits to exercise the reject
/// paths cheaply.
///
/// Built via [`UploadLimits::builder`]. Fields are private with defaults so new
/// caps can be added later without breaking callers (per the crate's API rules).
#[derive(Debug, Clone, Copy, bon::Builder)]
pub struct UploadLimits {
    /// Maximum size of a single uploaded trace (default 256 MiB).
    #[builder(default = 256 * 1024 * 1024)]
    max_upload_bytes: usize,
    /// Maximum total bytes held across all live uploads (default 1 GiB).
    #[builder(default = 1024 * 1024 * 1024)]
    max_total_bytes: usize,
    /// Maximum number of live uploads (default 100).
    #[builder(default = 100)]
    max_uploads: usize,
    /// How long an unfetched upload is retained before it is reclaimed
    /// (default 1 hour).
    #[builder(default = Duration::from_secs(60 * 60))]
    ttl: Duration,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl UploadLimits {
    /// Maximum size of a single uploaded trace, in bytes.
    pub fn max_upload_bytes(&self) -> usize {
        self.max_upload_bytes
    }
}

struct StoredUpload {
    bytes: Vec<u8>,
    expires_at: Instant,
}

/// In-memory store of temporary uploads, guarded by a `std::sync::Mutex`. The
/// lock is only ever held for synchronous map operations, never across `.await`.
pub struct UploadStore {
    limits: UploadLimits,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, StoredUpload>,
    total_bytes: usize,
}

/// Why an upload could not be accepted.
#[derive(Debug)]
enum InsertError {
    /// The store is at its byte or count capacity.
    CapacityExceeded,
}

impl UploadStore {
    pub fn new(limits: UploadLimits) -> Self {
        Self {
            limits,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn max_upload_bytes(&self) -> usize {
        self.limits.max_upload_bytes
    }

    /// Store `bytes` under a fresh id, returning the id. Purges expired entries
    /// first, then rejects if the new upload would exceed the byte or count cap.
    fn insert(&self, bytes: Vec<u8>, now: Instant) -> Result<String, InsertError> {
        let len = bytes.len();
        let mut inner = self.inner.lock().expect("upload store mutex poisoned");

        // Lazily reclaim anything that has expired before checking capacity.
        inner.purge_expired(now);

        // Per-upload size is also bounded at the HTTP layer (DefaultBodyLimit),
        // but enforce it here too so the store that owns the limit is the source
        // of truth and any future caller of `insert` stays bounded.
        if len > self.limits.max_upload_bytes
            || inner.entries.len() >= self.limits.max_uploads
            || inner.total_bytes.saturating_add(len) > self.limits.max_total_bytes
        {
            return Err(InsertError::CapacityExceeded);
        }

        // uuid v4 is collision-safe in practice; loop defensively rather than
        // risk silently overwriting (and leaking the byte count of) an entry.
        let id = loop {
            let candidate = uuid::Uuid::new_v4().to_string();
            if !inner.entries.contains_key(&candidate) {
                break candidate;
            }
        };

        inner.total_bytes += len;
        inner.entries.insert(
            id.clone(),
            StoredUpload {
                bytes,
                expires_at: now + self.limits.ttl,
            },
        );
        Ok(id)
    }

    /// Remove and return the upload for `id` if present and not expired
    /// (single-use). Returns `None` for unknown or expired ids.
    fn take(&self, id: &str, now: Instant) -> Option<Vec<u8>> {
        let mut inner = self.inner.lock().expect("upload store mutex poisoned");
        let entry = inner.entries.remove(id)?;
        inner.total_bytes -= entry.bytes.len();
        if entry.expires_at <= now {
            // Expired: we've already removed it, so just report it as gone.
            return None;
        }
        Some(entry.bytes)
    }
}

impl Inner {
    fn purge_expired(&mut self, now: Instant) {
        let mut freed = 0;
        self.entries.retain(|_, entry| {
            let keep = entry.expires_at > now;
            if !keep {
                freed += entry.bytes.len();
            }
            keep
        });
        self.total_bytes -= freed;
    }
}

#[derive(Serialize)]
struct UploadResponse {
    /// The generated id for the uploaded trace.
    id: String,
    /// Relative URL that serves the raw trace bytes (single-use).
    trace_url: String,
    /// Relative URL the caller can redirect users to in order to view the trace.
    viewer_url: String,
}

/// `POST /api/upload` — store the request body as a temporary trace.
///
/// The body must be a gzipped (`1f 8b`) or raw (`TRC\0`) trace. Returns JSON
/// with relative URLs; the caller prepends the viewer's origin (which it just
/// POSTed to). Relative URLs survive proxies and unknown public hostnames.
pub async fn upload_trace(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    if body.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "empty body".to_string()));
    }

    let is_gzip = body.starts_with(&GZIP_MAGIC);
    let is_trace = body.starts_with(&TRACE_MAGIC);
    if !is_gzip && !is_trace {
        return Err((
            StatusCode::BAD_REQUEST,
            "body is not a trace: expected gzip or 'TRC\\0' magic".to_string(),
        ));
    }

    // Routes are only mounted when uploads are enabled, so this is always
    // `Some` in practice; treat a missing store as the feature being off.
    let store = state
        .uploads
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "uploads disabled".to_string()))?;

    let now = Instant::now();
    let id = store
        .insert(body.to_vec(), now)
        .map_err(|InsertError::CapacityExceeded| {
            (
                StatusCode::INSUFFICIENT_STORAGE,
                "upload store is full; try again later".to_string(),
            )
        })?;

    let trace_url = format!("/api/uploaded/{id}");
    let viewer_url = format!("/viewer.html?trace={}", urlencode_component(&trace_url));
    tracing::info!(%id, bytes = body.len(), "stored temporary trace upload");

    let resp = UploadResponse {
        id,
        trace_url,
        viewer_url,
    };
    Ok((StatusCode::OK, axum::Json(resp)).into_response())
}

/// `GET /api/uploaded/{id}` — serve a previously uploaded trace, then delete it
/// (single-use). 404 if the id is unknown or expired.
pub async fn get_uploaded(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let store = state
        .uploads
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "uploads disabled".to_string()))?;

    match store.take(&id, Instant::now()) {
        Some(bytes) => Ok(Response::builder()
            .header("content-type", "application/octet-stream")
            .header("content-disposition", "attachment; filename=\"trace.bin\"")
            .body(Body::from(bytes))
            .expect("static headers and owned body are always valid")
            .into_response()),
        None => Err((StatusCode::NOT_FOUND, "no such upload".to_string())),
    }
}

/// Percent-encode the characters that matter inside a single query-param value.
/// Kept tiny and local so we don't pull in a URL-encoding dependency just to
/// escape a path we control (only `/` realistically appears).
fn urlencode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> UploadStore {
        UploadStore::new(UploadLimits::default())
    }

    #[test]
    fn insert_then_take_round_trips() {
        let s = store();
        let now = Instant::now();
        let id = s.insert(b"TRC\0hello".to_vec(), now).unwrap();
        assert_eq!(s.take(&id, now).as_deref(), Some(&b"TRC\0hello"[..]));
    }

    #[test]
    fn take_is_single_use() {
        let s = store();
        let now = Instant::now();
        let id = s.insert(b"TRC\0".to_vec(), now).unwrap();
        assert!(s.take(&id, now).is_some());
        assert!(s.take(&id, now).is_none());
    }

    #[test]
    fn expired_entry_is_not_returned() {
        let s = UploadStore::new(UploadLimits::builder().ttl(Duration::from_secs(10)).build());
        let now = Instant::now();
        let id = s.insert(b"TRC\0".to_vec(), now).unwrap();
        let later = now + Duration::from_secs(11);
        assert!(s.take(&id, later).is_none());
    }

    #[test]
    fn count_cap_rejects_then_purge_frees_room() {
        let s = UploadStore::new(
            UploadLimits::builder()
                .max_uploads(1)
                .ttl(Duration::from_secs(10))
                .build(),
        );
        let now = Instant::now();
        s.insert(b"TRC\0".to_vec(), now).unwrap();
        assert!(matches!(
            s.insert(b"TRC\0".to_vec(), now),
            Err(InsertError::CapacityExceeded)
        ));
        // Once the first entry expires, an insert purges it and succeeds.
        let later = now + Duration::from_secs(11);
        assert!(s.insert(b"TRC\0".to_vec(), later).is_ok());
    }

    #[test]
    fn total_bytes_cap_rejects() {
        let s = UploadStore::new(UploadLimits::builder().max_total_bytes(8).build());
        let now = Instant::now();
        s.insert(vec![0u8; 6], now).unwrap();
        assert!(matches!(
            s.insert(vec![0u8; 6], now),
            Err(InsertError::CapacityExceeded)
        ));
    }

    #[test]
    fn per_upload_size_cap_rejects() {
        let s = UploadStore::new(UploadLimits::builder().max_upload_bytes(8).build());
        let now = Instant::now();
        assert!(matches!(
            s.insert(vec![0u8; 9], now),
            Err(InsertError::CapacityExceeded)
        ));
        // At the limit is allowed.
        assert!(s.insert(vec![0u8; 8], now).is_ok());
    }

    #[test]
    fn total_bytes_decremented_on_take() {
        let s = store();
        let now = Instant::now();
        let id = s.insert(vec![0u8; 100], now).unwrap();
        s.take(&id, now);
        assert_eq!(s.inner.lock().unwrap().total_bytes, 0);
    }

    #[test]
    fn urlencode_escapes_slashes() {
        assert_eq!(
            urlencode_component("/api/uploaded/x"),
            "%2Fapi%2Fuploaded%2Fx"
        );
    }
}
