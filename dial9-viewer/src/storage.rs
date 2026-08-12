use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

#[cfg(feature = "s3")]
mod s3;
#[cfg(feature = "s3")]
pub use s3::{EphemeralS3Config, S3Backend, build_credentialed_client};

/// Metadata about an object in storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub key: String,
    pub size: i64,
    pub last_modified: Option<String>,
}

/// A bucket visible to the current credentials and its AWS region, when S3
/// included it in the `ListBuckets` response.
///
/// `#[non_exhaustive]` keeps this additive response type extensible without
/// preventing callers from reading the current fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BucketInfo {
    pub name: String,
    pub region: Option<String>,
}

impl BucketInfo {
    pub fn new(name: impl Into<String>, region: Option<String>) -> Self {
        Self {
            name: name.into(),
            region,
        }
    }
}

/// A bounded page of object listings, plus whether the cap was reached.
///
/// `truncated` is the signal the old listing path lacked: when a prefix fans
/// out to more objects than the cap, the listing stops early and the caller
/// (and ultimately the UI) needs to know data is missing rather than silently
/// showing a partial result.
///
/// This is the return type of [`StorageBackend::list_objects`], so it is
/// deliberately *not* `#[non_exhaustive]` — out-of-crate backends must be able
/// to construct it. Adding a field is therefore a breaking change.
#[derive(Debug, Clone)]
pub struct ListPage {
    pub objects: Vec<ObjectInfo>,
    /// True if the listing stopped at the requested cap and more objects exist
    /// that were not returned.
    pub truncated: bool,
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
    /// List the buckets the current credentials can see, including their AWS
    /// regions when available. Lets the viewer offer a region-aware bucket
    /// picker instead of requiring the user to know either value. Backends
    /// without a bucket concept (local FS) return an empty list.
    fn list_buckets(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<BucketInfo>, StorageError>> + Send + '_>>;

    /// List objects under `prefix`, paginating up to `cap` results.
    ///
    /// Returns the objects plus whether the listing was truncated at `cap` —
    /// the signal a caller needs so a partial result is never mistaken for a
    /// complete one. `/api/browse` fans many prefixes out at a high cap and
    /// surfaces the flag as a UI warning.
    fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        cap: usize,
    ) -> Pin<Box<dyn Future<Output = Result<ListPage, StorageError>> + Send + '_>>;

    /// List ALL objects under `prefix`, following pagination to exhaustion and
    /// without the cap that [`list_objects`] applies. Used by the ingest
    /// pipeline, which must discover every segment in a 24h window.
    ///
    /// [`list_objects`]: StorageBackend::list_objects
    fn list_objects_all(
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

    /// Put an object. Routes the write through this backend (S3 or local FS),
    /// so ingest output can target a different bucket/account/region than the
    /// source.
    fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + '_>>;

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
    /// The bucket lives in a different S3 region than the request was signed
    /// for (S3 `PermanentRedirect` / HTTP 301), and the correct region could
    /// not be resolved. Kept distinct from [`StorageError::Other`] so the HTTP
    /// layer returns a clear, actionable "wrong region" message instead of an
    /// opaque 500 — this is the failure the viewer's per-bucket region
    /// auto-detection exists to prevent.
    WrongRegion,
    /// The request was malformed by the client — e.g. an S3 `InvalidBucketName`
    /// for a bucket name that violates the naming rules (bad characters, wrong
    /// length). Kept distinct from [`StorageError::Other`] so the HTTP layer
    /// returns a `400` with an actionable message instead of an opaque `500`
    /// `fault`; the mistake is in the user's input, not the server.
    BadRequest(String),
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
            StorageError::WrongRegion => {
                write!(
                    f,
                    "this bucket is in a different AWS region than the request was \
                     signed for. Set the region (or pick the bucket so its region is \
                     detected automatically) and try again."
                )
            }
            StorageError::BadRequest(msg) => write!(f, "{msg}"),
            StorageError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for StorageError {}

#[cfg(unix)]
fn make_temporary_root_private(root: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn make_temporary_root_private(_root: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_atomically(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file_atomically(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain([0]).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: both arguments are valid NUL-terminated UTF-16 paths. The source
    // and destination are sibling files, so MoveFileExW performs one atomic
    // same-volume replacement and cannot expose a partial destination.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Local filesystem storage backend. Serves trace files from a directory.
///
/// The `bucket` parameter is ignored — all operations are relative to `root`.
/// Keys are relative paths from `root`.
pub struct LocalBackend {
    root: PathBuf,
    remove_on_drop: bool,
}

impl LocalBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        // Canonicalize root so that symlink resolution in child paths
        // (e.g. macOS /tmp → /private/tmp) matches the root prefix.
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root,
            remove_on_drop: false,
        }
    }

    /// Create a process-local aggregate store under the system temporary
    /// directory. The path is unique per backend and removed when the last
    /// owning `Arc` drops at server shutdown.
    pub(crate) fn new_temporary_aggregate() -> Self {
        Self::new_temporary_aggregate_in(std::env::temp_dir())
    }

    fn new_temporary_aggregate_in(base: impl AsRef<Path>) -> Self {
        // Resolve platform aliases before appending the not-yet-created unique
        // directory (notably macOS /tmp -> /private/tmp). Child canonicalization
        // in put_object can then be compared against this stable root.
        let base = base.as_ref();
        let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
        let root = base.join(format!(
            "dial9-aggregate-{}",
            uuid::Uuid::new_v4().as_hyphenated()
        ));
        Self {
            root,
            remove_on_drop: true,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for LocalBackend {
    fn drop(&mut self) {
        if !self.remove_on_drop {
            return;
        }
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    path = %self.root.display(),
                    error = %e,
                    "failed to remove temporary aggregate directory"
                );
            }
        }
    }
}

impl StorageBackend for LocalBackend {
    fn list_buckets(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<BucketInfo>, StorageError>> + Send + '_>> {
        // Local mode has no bucket concept; the synthetic "local" bucket is
        // wired in by the caller.
        Box::pin(async { Ok(Vec::new()) })
    }

    fn list_objects(
        &self,
        _bucket: &str,
        prefix: &str,
        cap: usize,
    ) -> Pin<Box<dyn Future<Output = Result<ListPage, StorageError>> + Send + '_>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let root = self.root.clone();
            let prefix2 = prefix.clone();
            tokio::task::spawn_blocking(move || {
                let mut objects = Vec::new();
                // `collect_files` enforces its own local-FS-safety bounds
                // (`MAX_COLLECT_FILES`); the `cap` is applied on top so the
                // contract holds for any caller-chosen limit.
                collect_files(&root, &root, &prefix2, &mut objects, 0, &mut 0)?;
                objects.sort_by(|a, b| a.key.cmp(&b.key));
                let truncated = objects.len() > cap;
                objects.truncate(cap);
                Ok(ListPage { objects, truncated })
            })
            .await
            .map_err(|e| StorageError::Other(e.to_string()))?
        })
    }

    fn list_objects_all(
        &self,
        _bucket: &str,
        prefix: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ObjectInfo>, StorageError>> + Send + '_>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let root = self.root.clone();
            tokio::task::spawn_blocking(move || {
                // Uncapped recursive walk: ingest needs every segment, unlike
                // the UI listing which caps via `collect_files`.
                let mut objects = Vec::new();
                collect_files_uncapped(&root, &root, &prefix, &mut objects)?;
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

    fn put_object(
        &self,
        _bucket: &str,
        key: &str,
        data: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + '_>> {
        let root = self.root.clone();
        let private_temporary_root = self.remove_on_drop;
        let key = key.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let path = root.join(&key);
                // Reject path-traversal keys *before* creating any directories.
                // `create_dir_all` would otherwise materialize `../` directories
                // outside the root before the canonicalize check below could
                // reject the write, leaving stray dirs behind.
                if path.components().any(|c| c == Component::ParentDir) {
                    return Err(StorageError::Other(
                        "key contains path traversal".to_string(),
                    ));
                }
                let parent = path.parent().ok_or_else(|| {
                    StorageError::Other("object path has no parent directory".to_string())
                })?;
                std::fs::create_dir_all(parent).map_err(|e| StorageError::Other(e.to_string()))?;
                if private_temporary_root {
                    make_temporary_root_private(&root)
                        .map_err(|e| StorageError::Other(e.to_string()))?;
                }
                let canonical_parent = parent
                    .canonicalize()
                    .map_err(|e| StorageError::Other(e.to_string()))?;
                if !canonical_parent.starts_with(&root) {
                    return Err(StorageError::Other(
                        "path escapes root directory".to_string(),
                    ));
                }

                // Publish through a hidden same-directory temporary file. A
                // direct write to `path` would expose a truncated final object
                // to concurrent aggregate readers; rename gives local storage
                // the same all-at-once visibility that an S3 PutObject has.
                let temp_path = canonical_parent.join(format!(
                    ".dial9-write-{}",
                    uuid::Uuid::new_v4().as_hyphenated()
                ));
                std::fs::write(&temp_path, data).map_err(|e| StorageError::Other(e.to_string()))?;
                if let Err(rename_error) = replace_file_atomically(&temp_path, &path) {
                    if let Err(cleanup_error) = std::fs::remove_file(&temp_path)
                        && cleanup_error.kind() != std::io::ErrorKind::NotFound
                    {
                        tracing::warn!(
                            path = %temp_path.display(),
                            error = %cleanup_error,
                            "failed to clean up temporary object write"
                        );
                    }
                    return Err(StorageError::Other(rename_error.to_string()));
                }
                Ok(())
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

/// Recursively collect ALL files under `root` whose key starts with `prefix`,
/// with NO file-count or entries-visited caps. Used by
/// [`LocalBackend::list_objects_all`] for ingest, which must see the whole tree.
///
/// This mirrors S3 `list_objects_all` semantics: it returns *every* matching
/// object (including the aggregation output `.parquet` files under `samples/`,
/// `dict/`, and `polls/`), so callers like the folded-set listing
/// ([`crate::ingest::aggregate::list_folded_leaves`]) see the whole output tree.
/// Trace-file selection (the `.bin` / `.bin.gz` extension filter) is the
/// caller's responsibility — the ingest lister applies it. Honors the same
/// path-escape safety (canonical paths must stay within `root`) and skipped
/// directories (`.`, `target`, `node_modules`) as [`collect_files`].
fn collect_files_uncapped(
    root: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<ObjectInfo>,
) -> Result<(), StorageError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(StorageError::Other("permission denied".into()));
        }
        Err(e) => return Err(StorageError::Other(e.to_string())),
    };
    for entry in entries {
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
                collect_files_uncapped(root, &canonical, prefix, out)?;
            }
        } else if canonical.is_file() {
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

    /// A `put_object` key containing `../` must be rejected *before* any
    /// directories are created — otherwise the traversal materializes dirs
    /// outside the root that the later canonicalize check never cleans up.
    #[tokio::test]
    async fn put_object_rejects_path_traversal_without_creating_dirs() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let backend = LocalBackend::new(&root);

        let err = backend
            .put_object("bucket", "../escape/evil.bin", b"x".to_vec())
            .await
            .expect_err("traversal key must be rejected");
        match err {
            StorageError::Other(msg) => {
                assert!(msg.contains("path traversal"), "unexpected message: {msg}")
            }
            other => panic!("expected StorageError::Other, got {other:?}"),
        }

        // The traversal dir (`<outer>/escape`) must NOT have been created.
        let escape_dir = outer.path().join("escape");
        assert!(
            !escape_dir.exists(),
            "path traversal created a directory outside the root: {}",
            escape_dir.display()
        );
    }

    /// A normal key writes through `put_object`, creating parent dirs under root.
    #[tokio::test]
    async fn put_object_writes_normal_key() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalBackend::new(dir.path());

        backend
            .put_object("bucket", "a/b/c.bin", b"hi".to_vec())
            .await
            .unwrap();

        let written = dir.path().join("a/b/c.bin");
        assert!(written.exists(), "expected file at {}", written.display());
        assert_eq!(std::fs::read(&written).unwrap(), b"hi");
    }

    /// Concurrent overwrites of one object must never expose a truncated body.
    /// Aggregate folds use deterministic keys, so overlapping requests can hit
    /// this exact pattern.
    #[tokio::test]
    async fn concurrent_put_object_readers_see_only_complete_versions() {
        let dir = tempfile::tempdir().unwrap();
        let backend = std::sync::Arc::new(LocalBackend::new(dir.path()));
        let key = "samples/part.parquet";
        let body_len = 1024 * 1024;
        backend
            .put_object("local", key, vec![0; body_len])
            .await
            .unwrap();

        let mut writers = Vec::new();
        for byte in 1..=8u8 {
            let backend = std::sync::Arc::clone(&backend);
            writers.push(tokio::spawn(async move {
                backend
                    .put_object("local", key, vec![byte; body_len])
                    .await
                    .unwrap();
            }));
        }

        for _ in 0..64 {
            let body = backend.get_object("local", key).await.unwrap();
            assert_eq!(body.len(), body_len, "reader observed a truncated object");
            assert!(
                body.iter().all(|byte| *byte == body[0]),
                "reader observed bytes from multiple object versions"
            );
            tokio::task::yield_now().await;
        }
        for writer in writers {
            writer.await.unwrap();
        }
    }

    /// A temporary aggregate backend removes its whole cache tree when dropped.
    #[tokio::test]
    async fn temporary_aggregate_backend_cleans_up_on_drop() {
        let backend = LocalBackend::new_temporary_aggregate();
        let root = backend.root.clone();
        backend
            .put_object("cache", "a/b/c.parquet", b"cached".to_vec())
            .await
            .unwrap();
        assert!(root.join("a/b/c.parquet").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "temporary cache root must be owner-only");
        }

        drop(backend);
        assert!(!root.exists(), "temporary aggregate directory leaked");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn temporary_aggregate_backend_resolves_symlinked_temp_base() {
        use std::os::unix::fs::symlink;

        let outer = tempfile::tempdir().unwrap();
        let real_base = outer.path().join("real-temp");
        let linked_base = outer.path().join("temp-link");
        std::fs::create_dir(&real_base).unwrap();
        symlink(&real_base, &linked_base).unwrap();

        let backend = LocalBackend::new_temporary_aggregate_in(&linked_base);
        assert!(backend.root.starts_with(real_base.canonicalize().unwrap()));
        backend
            .put_object("cache", "samples/part.parquet", b"complete".to_vec())
            .await
            .unwrap();
        assert_eq!(
            backend
                .get_object("cache", "samples/part.parquet")
                .await
                .unwrap(),
            b"complete"
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
