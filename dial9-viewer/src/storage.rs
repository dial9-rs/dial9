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
    let mut cfg = aws_sdk_s3::config::Builder::new()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .credentials_provider(credentials)
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
            let mut names = Vec::new();
            let mut continuation: Option<String> = None;
            loop {
                let mut req = self.client.list_buckets();
                if let Some(token) = continuation.take() {
                    req = req.continuation_token(token);
                }
                let resp = req.send().await.map_err(|e| classify_s3_error(&e))?;
                for b in resp.buckets() {
                    if let Some(name) = b.name() {
                        names.push(name.to_string());
                    }
                }
                match resp.continuation_token() {
                    Some(token) if !token.is_empty() => continuation = Some(token.to_string()),
                    _ => break,
                }
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
            let mut objects = Vec::new();
            let mut continuation: Option<String> = None;

            loop {
                let mut req = self
                    .client
                    .list_objects_v2()
                    .bucket(&bucket)
                    .prefix(&prefix);
                if let Some(token) = continuation.take() {
                    req = req.continuation_token(token);
                }

                let resp = req.send().await.map_err(|e| classify_s3_error(&e))?;

                for obj in resp.contents() {
                    if let Some(key) = obj.key() {
                        objects.push(ObjectInfo {
                            key: key.to_string(),
                            size: obj.size().unwrap_or(0),
                            last_modified: obj.last_modified().map(|t| t.to_string()),
                        });
                    }
                }

                if objects.len() >= MAX_RESULTS {
                    objects.truncate(MAX_RESULTS);
                    break;
                }

                if resp.is_truncated() == Some(true) {
                    continuation = resp.next_continuation_token().map(|s| s.to_string());
                } else {
                    break;
                }
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
            let resp = self
                .client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(&prefix)
                .delimiter("/")
                .send()
                .await
                .map_err(|e| classify_s3_error(&e))?;

            Ok(resp
                .common_prefixes()
                .iter()
                .filter_map(|cp| cp.prefix().map(|s| s.to_string()))
                .collect())
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
