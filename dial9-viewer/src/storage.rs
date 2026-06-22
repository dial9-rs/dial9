use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// Metadata about an object in storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub key: String,
    pub size: i64,
    pub last_modified: Option<String>,
}

/// A handle to an object's bytes that can be streamed to the client as they
/// arrive, rather than buffered in full first.
///
/// This exists to remove the time-to-first-byte (TTFB) stall on `/api/object`:
/// the old buffered path called `ByteStream::collect()`, which pulled the entire
/// object out of S3 into a `Vec<u8>` before a single byte could be written to
/// the browser (measured ~2s TTFB on real traces). With a streamed body, bytes
/// flow to the browser as S3 delivers them, so the server↔S3 download overlaps
/// with the browser↔server transfer (and the browser's incremental
/// gunzip+decode in `fetchTraceStream`).
///
/// The chunk error type is [`std::io::Error`] so the stream composes directly
/// with [`axum::body::Body::from_stream`] (whose error bound is
/// `Into<BoxError>`).
///
/// `#[non_exhaustive]`: adding a field later (e.g. `content_type`) must not be a
/// breaking change for out-of-crate `StorageBackend` implementors. It is only
/// ever constructed inside this crate, where struct-literal construction still
/// works.
#[non_exhaustive]
pub struct ObjectStream {
    /// The object's bytes, chunk by chunk, as they arrive from the backend.
    pub stream: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    /// The object's total size, if known up front (S3 returns `Content-Length`
    /// on `GetObject`). Forwarded as the response `content-length` header so the
    /// browser can show real download progress.
    pub content_length: Option<i64>,
}

/// Abstraction over trace storage (S3, local FS, etc.)
pub trait StorageBackend: Send + Sync {
    /// List the buckets the current credentials can see. Lets the viewer offer
    /// a bucket picker instead of requiring the user to know the name. Backends
    /// without a bucket concept (local FS) return an empty list.
    fn list_buckets(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>>;

    fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>>;

    /// List immediate child prefixes under `prefix` using delimiter-based listing.
    fn list_prefixes(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>>;

    fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>>;

    /// Like [`get_object`](StorageBackend::get_object), but returns the body as a
    /// stream so the HTTP layer can forward bytes to the client as they arrive
    /// instead of buffering the whole object first. See [`ObjectStream`] for the
    /// TTFB rationale.
    ///
    /// Setup errors (object not found, auth failure, etc.) surface from the
    /// returned future *before* any body streams — so the HTTP layer can still
    /// map them to a 404/401/403 status. Errors encountered mid-stream (after
    /// the status line and headers have already been sent) arrive as
    /// `Err(io::Error)` items on the stream and can no longer change the status.
    ///
    /// The default implementation buffers via `get_object` and wraps the result
    /// in a single-chunk stream. This is correct (just not incremental) and is
    /// the right behavior for backends with no TTFB problem — e.g. local file
    /// reads — so [`LocalBackend`] and test backends need no override.
    fn get_object_stream(
        &self,
        bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<ObjectStream, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        Box::pin(async move {
            let data = self.get_object(&bucket, &key).await?;
            let content_length = Some(data.len() as i64);
            let stream = futures::stream::once(async move { Ok(Bytes::from(data)) });
            Ok(ObjectStream {
                stream: Box::pin(stream),
                content_length,
            })
        })
    }
}

#[derive(Debug)]
pub enum StorageError {
    NotFound(String),
    /// The credentials were rejected by S3 (bad keys, wrong region, expired
    /// token, access denied). Kept distinct from [`StorageError::Other`] so the
    /// HTTP layer can return a generic 401 without echoing the underlying SDK
    /// message — which can contain the access key id.
    Unauthorized,
    /// The AWS account behind the credentials is not signed up for / opted in
    /// to S3 in this region. Almost always means the request was signed by the
    /// *wrong* identity (e.g. the server's ambient credentials instead of the
    /// pasted ones), so the message points the user there.
    AccountNotSignedUp,
    Other(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::NotFound(msg) => write!(f, "not found: {msg}"),
            StorageError::Unauthorized => {
                write!(
                    f,
                    "credentials rejected by S3 (check keys, region, or expiry)"
                )
            }
            StorageError::AccountNotSignedUp => {
                write!(
                    f,
                    "the AWS account used for this request is not signed up for S3 — \
                     this usually means the request was signed with the wrong identity. \
                     Make sure you clicked Apply after pasting your credentials."
                )
            }
            StorageError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// Map an S3 SDK error to a [`StorageError`], collapsing all
/// authentication/authorization failures to [`StorageError::Unauthorized`] so
/// the secret, token, and access key id are never reflected to the client.
///
/// Uses the structured error code (via `ProvideErrorMetadata`) rather than
/// string matching, plus the HTTP status as a backstop.
fn classify_s3_error<E, R>(err: &aws_sdk_s3::error::SdkError<E, R>) -> StorageError
where
    E: std::error::Error + aws_sdk_s3::error::ProvideErrorMetadata + 'static,
    R: std::fmt::Debug,
{
    use aws_sdk_s3::error::ProvideErrorMetadata;
    match err.code() {
        Some(
            "InvalidAccessKeyId"
            | "SignatureDoesNotMatch"
            | "ExpiredToken"
            | "ExpiredTokenException"
            | "InvalidToken"
            | "AccessDenied"
            | "AccessDeniedException"
            | "UnrecognizedClientException"
            | "InvalidClientTokenId"
            | "AuthorizationHeaderMalformed",
        ) => StorageError::Unauthorized,
        // Account-level: the credentials are valid but the account isn't signed
        // up for S3 in this region — typically the wrong identity signed it.
        Some("NotSignedUp" | "OptInRequired") => StorageError::AccountNotSignedUp,
        // Unmapped error: keep the full SDK detail in the server log (it can
        // embed the access key id, region, and endpoint — server-eyes only) and
        // hand the client a generic message rather than reflecting it back.
        _ => {
            tracing::warn!(
                error = %aws_sdk_s3::error::DisplayErrorContext(err),
                "unclassified S3 error"
            );
            StorageError::Other("could not complete the S3 request".to_string())
        }
    }
}

/// Optional plumbing for building ephemeral (bring-your-own-credentials) S3
/// clients. In production this is `None` and clients use the default HTTPS
/// connector. Tests inject the in-process `s3s` HTTP client plus an endpoint
/// override so the header → ephemeral-client → fake-S3 path is exercisable.
///
/// This is a test seam, not part of the public API surface — it is `pub` only
/// so integration tests in another crate can construct it.
#[doc(hidden)]
#[derive(Clone)]
pub struct EphemeralS3Config {
    /// Shared HTTP client/connector reused across ephemeral clients.
    pub http_client: aws_sdk_s3::config::SharedHttpClient,
    /// Endpoint override (test-only — never wired to user input; that would be
    /// an SSRF vector).
    pub endpoint_url: Option<String>,
    /// Path-style addressing — required by the `s3s` fake, never for real S3.
    pub force_path_style: bool,
}

/// Default region used when the user did not supply (and we could not detect)
/// one. S3 routes bucket operations regardless once the bucket region is known,
/// but a concrete region is required to build the client.
const DEFAULT_REGION: &str = "us-east-1";

/// Per-attempt timeout: how long a single HTTP attempt may take before the SDK
/// gives up on it (and possibly retries).
const OPERATION_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Overall operation timeout: the wall-clock budget for an entire S3 call,
/// including all retries. Bounds how long a request to a wrong region, a
/// black-holed endpoint, or unresponsive S3 can hang the viewer's request
/// handler.
const OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// S3-backed storage using the AWS SDK.
pub struct S3Backend {
    client: aws_sdk_s3::Client,
}

impl S3Backend {
    pub async fn from_env() -> Self {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Self {
            client: aws_sdk_s3::Client::new(&config),
        }
    }

    /// Create from an existing S3 client (useful for testing with s3s).
    pub fn from_client(client: aws_sdk_s3::Client) -> Self {
        Self { client }
    }

    /// Build an ephemeral backend from user-supplied credentials.
    ///
    /// The credentials are passed as a concrete value, which acts as a *static*
    /// credential provider: it can never fall back to the server's IMDS/env
    /// identity. That is the core security property of bring-your-own-creds.
    pub fn from_credentials(
        credentials: aws_sdk_s3::config::Credentials,
        region: Option<&str>,
        ephemeral: &Option<EphemeralS3Config>,
    ) -> Self {
        Self::from_client(build_credentialed_client(credentials, region, ephemeral))
    }
}

/// Construct an `aws_sdk_s3::Client` from explicit credentials. Shared by the
/// ephemeral backend and the `/api/credentials/check` validation handler.
pub fn build_credentialed_client(
    credentials: aws_sdk_s3::config::Credentials,
    region: Option<&str>,
    ephemeral: &Option<EphemeralS3Config>,
) -> aws_sdk_s3::Client {
    let region = region.unwrap_or(DEFAULT_REGION).to_string();
    let timeouts = aws_sdk_s3::config::timeout::TimeoutConfig::builder()
        .operation_attempt_timeout(OPERATION_ATTEMPT_TIMEOUT)
        .operation_timeout(OPERATION_TIMEOUT)
        .build();
    let mut cfg = aws_sdk_s3::config::Builder::new()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .credentials_provider(credentials)
        .timeout_config(timeouts)
        .region(aws_sdk_s3::config::Region::new(region));

    if let Some(e) = ephemeral {
        cfg = cfg.http_client(e.http_client.clone());
        if let Some(url) = &e.endpoint_url {
            cfg = cfg.endpoint_url(url);
        }
        if e.force_path_style {
            cfg = cfg.force_path_style(true);
        }
    }

    aws_sdk_s3::Client::from_conf(cfg.build())
}

impl StorageBackend for S3Backend {
    fn list_buckets(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        Box::pin(async move {
            const MAX_BUCKETS: usize = 200;
            let mut pages = self.client.list_buckets().into_paginator().send();
            let mut names = Vec::new();
            let mut truncated = false;
            'pages: while let Some(page) = pages.next().await {
                let page = page.map_err(|e| classify_s3_error(&e))?;
                for b in page.buckets() {
                    if let Some(name) = b.name() {
                        names.push(name.to_string());
                    }
                    if names.len() >= MAX_BUCKETS {
                        truncated = true;
                        break 'pages;
                    }
                }
            }
            if truncated {
                tracing::warn!(
                    max = MAX_BUCKETS,
                    "bucket listing truncated at cap; some buckets are not shown"
                );
            }
            names.sort();
            Ok(names)
        })
    }

    fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let prefix = prefix.to_string();
        Box::pin(async move {
            const MAX_RESULTS: usize = 1000;
            let mut pages = self
                .client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(&prefix)
                .into_paginator()
                .send();

            let mut objects = Vec::new();
            let mut truncated = false;
            'pages: while let Some(page) = pages.next().await {
                let page = page.map_err(|e| classify_s3_error(&e))?;
                for obj in page.contents() {
                    if let Some(key) = obj.key() {
                        objects.push(ObjectInfo {
                            key: key.to_string(),
                            size: obj.size().unwrap_or(0),
                            last_modified: obj.last_modified().map(|t| t.to_string()),
                        });
                    }
                    if objects.len() >= MAX_RESULTS {
                        truncated = true;
                        break 'pages;
                    }
                }
            }
            if truncated {
                tracing::warn!(
                    bucket = %bucket,
                    prefix = %prefix,
                    max = MAX_RESULTS,
                    "object listing truncated at cap; some objects are not shown"
                );
            }

            Ok(objects)
        })
    }

    fn list_prefixes(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let prefix = prefix.to_string();
        Box::pin(async move {
            // Bound the number of child prefixes returned, mirroring the caps on
            // the other listings, so a directory with an unbounded fan-out can't
            // produce an enormous response.
            const MAX_PREFIXES: usize = 1000;
            // Common prefixes count against MaxKeys per response, so a directory
            // with more than one page of children must be paginated or it would
            // silently truncate.
            let mut pages = self
                .client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(&prefix)
                .delimiter("/")
                .into_paginator()
                .send();

            let mut prefixes = Vec::new();
            let mut truncated = false;
            'pages: while let Some(page) = pages.next().await {
                let page = page.map_err(|e| classify_s3_error(&e))?;
                for cp in page.common_prefixes() {
                    if let Some(p) = cp.prefix() {
                        prefixes.push(p.to_string());
                    }
                    if prefixes.len() >= MAX_PREFIXES {
                        truncated = true;
                        break 'pages;
                    }
                }
            }
            if truncated {
                tracing::warn!(
                    bucket = %bucket,
                    prefix = %prefix,
                    max = MAX_PREFIXES,
                    "prefix listing truncated at cap; some child prefixes are not shown"
                );
            }
            Ok(prefixes)
        })
    }

    fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        Box::pin(async move {
            let resp = self
                .client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    use aws_sdk_s3::operation::get_object::GetObjectError;

                    // Classify before unwrapping the service error so auth
                    // failures (which arrive as the redirect/4xx service error)
                    // collapse to Unauthorized rather than leaking the message.
                    let classified = classify_s3_error(&e);
                    match e.into_service_error() {
                        GetObjectError::NoSuchKey(_) => {
                            StorageError::NotFound(format!("{bucket}/{key}"))
                        }
                        _ => classified,
                    }
                })?;

            let bytes = resp
                .body
                .collect()
                .await
                .map_err(|e| StorageError::Other(e.to_string()))?;

            Ok(bytes.to_vec())
        })
    }

    /// Stream the object body straight from S3 instead of buffering it. The
    /// `GetObject` request (and thus NoSuchKey / auth / not-found classification)
    /// still completes synchronously in the returned future, so the HTTP layer
    /// gets the right status before any body streams. The body is then handed
    /// back as a chunk stream — we do NOT call `.collect()`.
    fn get_object_stream(
        &self,
        bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<ObjectStream, StorageError>> + Send + '_>> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        Box::pin(async move {
            let resp = self
                .client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    use aws_sdk_s3::operation::get_object::GetObjectError;

                    // Classify before unwrapping the service error so auth
                    // failures (which arrive as the redirect/4xx service error)
                    // collapse to Unauthorized rather than leaking the message.
                    let classified = classify_s3_error(&e);
                    match e.into_service_error() {
                        GetObjectError::NoSuchKey(_) => {
                            StorageError::NotFound(format!("{bucket}/{key}"))
                        }
                        _ => classified,
                    }
                })?;

            let content_length = resp.content_length();

            // Drive the body via the always-public `ByteStream::next()` (the
            // `Stream` impl on `ByteStream` itself is feature-gated/private in
            // this SDK). `unfold` yields one chunk per poll, mapping the SDK
            // chunk error into `io::Error` so the stream composes with
            // `Body::from_stream`. No `.collect()` — bytes flow as they arrive.
            let stream = futures::stream::unfold(resp.body, |mut body| async move {
                match body.next().await {
                    Some(Ok(chunk)) => Some((Ok(chunk), body)),
                    Some(Err(e)) => Some((Err(std::io::Error::other(e)), body)),
                    None => None,
                }
            });

            Ok(ObjectStream {
                stream: Box::pin(stream),
                content_length,
            })
        })
    }
}

/// Local filesystem storage backend. Serves trace files from a directory.
///
/// The `bucket` parameter is ignored — all operations are relative to `root`.
/// Keys are relative paths from `root`.
pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        // Canonicalize root so that symlink resolution in child paths
        // (e.g. macOS /tmp → /private/tmp) matches the root prefix.
        let root = root.canonicalize().unwrap_or(root);
        Self { root }
    }
}

impl StorageBackend for LocalBackend {
    fn list_buckets(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        // Local mode has no bucket concept; the synthetic "local" bucket is
        // wired in by the caller.
        Box::pin(async { Ok(Vec::new()) })
    }

    fn list_objects(
        &self,
        _bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let root = self.root.clone();
            let prefix2 = prefix.clone();
            tokio::task::spawn_blocking(move || {
                let mut objects = Vec::new();
                collect_files(&root, &root, &prefix2, &mut objects, 0, &mut 0)?;
                objects.sort_by(|a, b| a.key.cmp(&b.key));
                Ok(objects)
            })
            .await
            .map_err(|e| StorageError::Other(e.to_string()))?
        })
    }

    fn list_prefixes(
        &self,
        _bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, StorageError>> + Send + '_>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let root = self.root.clone();
            let prefix2 = prefix.clone();
            tokio::task::spawn_blocking(move || {
                let dir = root.join(&prefix2);
                let dir = match dir.canonicalize() {
                    Ok(d) if d.starts_with(&root) => d,
                    Ok(_) => {
                        return Err(StorageError::NotFound(
                            "path escapes root directory".to_string(),
                        ));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                    Err(e) => return Err(StorageError::Other(e.to_string())),
                };
                let entries = match std::fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                    Err(e) => return Err(StorageError::Other(e.to_string())),
                };
                let mut prefixes = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(|e| StorageError::Other(e.to_string()))?;
                    let path = entry.path();
                    // Resolve symlinks and verify the target stays within root.
                    let canonical = match path.canonicalize() {
                        Ok(c) if c.starts_with(&root) => c,
                        _ => continue,
                    };
                    if canonical.is_dir() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        // NOTE: This uses "/" unconditionally, matching S3 key semantics.
                        // On Windows, this would need to use the platform separator or
                        // normalize paths to forward slashes throughout.
                        prefixes.push(format!("{prefix2}{name}/"));
                    }
                }
                prefixes.sort();
                Ok(prefixes)
            })
            .await
            .map_err(|e| StorageError::Other(e.to_string()))?
        })
    }

    fn get_object(
        &self,
        _bucket: &str,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, StorageError>> + Send + '_>> {
        let path = self.root.join(key);
        let root = self.root.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let canonical = path.canonicalize().map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => {
                        StorageError::NotFound(path.display().to_string())
                    }
                    _ => StorageError::Other(e.to_string()),
                })?;
                if !canonical.starts_with(&root) {
                    return Err(StorageError::NotFound(
                        "path escapes root directory".to_string(),
                    ));
                }
                std::fs::read(&canonical).map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => {
                        StorageError::NotFound(path.display().to_string())
                    }
                    _ => StorageError::Other(e.to_string()),
                })
            })
            .await
            .map_err(|e| StorageError::Other(e.to_string()))?
        })
    }
}

/// Maximum directory depth to recurse into when listing local files.
const MAX_COLLECT_DEPTH: u32 = 10;

/// Maximum number of files to return from a local directory listing.
const MAX_COLLECT_FILES: usize = 50;

/// Maximum number of directory entries to visit (files + dirs) across the
/// entire recursive walk. This bounds the number of syscalls (`canonicalize`,
/// `metadata`) so a huge directory tree cannot hang the listing.
const MAX_ENTRIES_VISITED: usize = 500;

/// Directory names to skip during recursive file collection.
fn is_skipped_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "target" | "node_modules")
}

fn collect_files(
    root: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<ObjectInfo>,
    depth: u32,
    visited: &mut usize,
) -> Result<(), StorageError> {
    if depth > MAX_COLLECT_DEPTH
        || out.len() >= MAX_COLLECT_FILES
        || *visited >= MAX_ENTRIES_VISITED
    {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(StorageError::Other("permission denied".into()));
        }
        Err(e) => return Err(StorageError::Other(e.to_string())),
    };
    for entry in entries {
        *visited += 1;
        if out.len() >= MAX_COLLECT_FILES || *visited >= MAX_ENTRIES_VISITED {
            break;
        }
        let entry = entry.map_err(|e| StorageError::Other(e.to_string()))?;
        let path = entry.path();
        // Resolve symlinks and verify the target stays within root.
        let canonical = match path.canonicalize() {
            Ok(c) if c.starts_with(root) => c,
            _ => continue,
        };
        if canonical.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !is_skipped_dir(&name) {
                collect_files(root, &canonical, prefix, out, depth + 1, visited)?;
            }
        } else if canonical.is_file() {
            let file_name = entry.file_name();
            let file_name_str = file_name.to_string_lossy();
            if file_name_str.starts_with('.') {
                continue;
            }
            let key = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if key.starts_with(prefix) {
                let meta = std::fs::metadata(&canonical)
                    .map_err(|e| StorageError::Other(e.to_string()))?;
                out.push(ObjectInfo {
                    key,
                    size: meta.len() as i64,
                    last_modified: meta.modified().ok().and_then(|t| {
                        t.duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs().to_string())
                    }),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `S3Backend` whose HTTP layer replays the given canned responses
    /// in order, so multi-page pagination can be tested without a live S3 (the
    /// `s3s-fs` fake never emits a continuation token, so it can't drive page 2).
    fn replay_backend(
        responses: Vec<&str>,
    ) -> (
        S3Backend,
        aws_smithy_http_client::test_util::StaticReplayClient,
    ) {
        use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
        use aws_smithy_types::body::SdkBody;

        let events = responses
            .into_iter()
            .map(|body| {
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://s3.amazonaws.com/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    http::Response::builder()
                        .status(200)
                        .body(SdkBody::from(body))
                        .unwrap(),
                )
            })
            .collect();
        let http_client = StaticReplayClient::new(events);
        let cfg = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(http_client.clone())
            .build();
        (
            S3Backend::from_client(aws_sdk_s3::Client::from_conf(cfg)),
            http_client,
        )
    }

    #[tokio::test]
    async fn list_prefixes_follows_continuation_token() {
        // Page 1 is truncated and carries a NextContinuationToken; page 2 is the
        // final page. The fix must follow the token and merge both pages — the
        // old single-shot code would drop `c/`.
        let page1 = r#"<?xml version="1.0" encoding="UTF-8"?>
            <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <Name>bucket</Name><Prefix></Prefix><Delimiter>/</Delimiter>
              <IsTruncated>true</IsTruncated>
              <NextContinuationToken>TOKEN_A</NextContinuationToken>
              <CommonPrefixes><Prefix>a/</Prefix></CommonPrefixes>
              <CommonPrefixes><Prefix>b/</Prefix></CommonPrefixes>
            </ListBucketResult>"#;
        let page2 = r#"<?xml version="1.0" encoding="UTF-8"?>
            <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <Name>bucket</Name><Prefix></Prefix><Delimiter>/</Delimiter>
              <IsTruncated>false</IsTruncated>
              <CommonPrefixes><Prefix>c/</Prefix></CommonPrefixes>
            </ListBucketResult>"#;

        let (backend, http_client) = replay_backend(vec![page1, page2]);
        let prefixes = backend.list_prefixes("bucket", "").await.unwrap();
        assert_eq!(prefixes, vec!["a/", "b/", "c/"]);

        // Two HTTP calls were made, and the second carried the continuation token
        // from the first — proving pagination actually happened.
        let requests = http_client.actual_requests().collect::<Vec<_>>();
        assert_eq!(requests.len(), 2, "expected two list calls");
        assert!(
            requests[1].uri().contains("continuation-token=TOKEN_A"),
            "second request must carry the continuation token, got: {}",
            requests[1].uri()
        );
    }

    #[test]
    fn collect_files_caps_entries_visited() {
        let dir = tempfile::tempdir().unwrap();
        // Create more files than MAX_ENTRIES_VISITED to prove we stop early.
        let n = MAX_ENTRIES_VISITED + 500;
        for i in 0..n {
            std::fs::write(dir.path().join(format!("file_{i:05}.bin")), b"x").unwrap();
        }
        let mut out = Vec::new();
        let mut visited = 0;
        collect_files(dir.path(), dir.path(), "", &mut out, 0, &mut visited).unwrap();
        // visited must be capped — we should NOT have iterated all n files.
        assert!(
            visited <= MAX_ENTRIES_VISITED,
            "visited {visited} entries, expected at most {MAX_ENTRIES_VISITED}"
        );
        assert!(
            out.len() <= MAX_COLLECT_FILES,
            "collected {} files, expected at most {MAX_COLLECT_FILES}",
            out.len()
        );
    }
}
