//! S3 uploader for sealed trace segments.
//!
//! Uploads processed segment bytes to S3 with a single `PutObject` per segment.
//! Deletes local files only after confirmed upload.

use crate::connection;
pub use crate::instance_metadata::InstanceIdentity;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::put_object::PutObjectError;
use dial9_core::boot_id::generate_boot_id as default_boot_id;
use dial9_core::pipeline::{
    ProcessError, ProcessErrorKind, SegmentData, SegmentProcessor, SegmentRef,
};
use dial9_core::rate_limited;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Classify a `PutObject` `SdkError`'s retryability and wrap it as a
/// [`ProcessErrorKind::transfer`]. A free fn rather than `impl From` because the
/// orphan rule forbids implementing a foreign trait for a foreign type here.
///
/// Transport-level failures (timeouts, dispatch/IO, unparseable responses) are
/// transient and worth retrying. For a service error we keep the segment when
/// the response is a 5xx or a throttle (429), and give up on a 4xx (auth,
/// permission, malformed request) — retrying those would only spin.
fn put_error_kind(e: SdkError<PutObjectError>) -> ProcessErrorKind {
    let retryable = match &e {
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) | SdkError::ResponseError(_) => {
            true
        }
        SdkError::ServiceError(ctx) => {
            let status = ctx.raw().status().as_u16();
            status >= 500 || status == 429
        }
        // ConstructionFailure and any future non-exhaustive variant: not worth
        // retrying (the request never made it onto the wire coherently).
        _ => false,
    };
    ProcessErrorKind::transfer(Box::new(e), retryable)
}

/// Metadata about a sealed trace segment, passed to custom key functions.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SegmentInfo {
    /// The segment index (e.g. 3 for `trace.3.bin`).
    pub index: u32,
    /// Segment creation time as seconds since the Unix epoch.
    pub epoch_secs: u64,
    /// Identifier for this process lifetime. A new value each application
    /// start, so segment indices from different runs do not collide.
    pub boot_id: String,
}

/// Trait for custom S3 object key generation.
///
/// Implement this to control the S3 key layout. The default key layout is
/// `{prefix}/{date}/{HHMM}/{service}/{instance}/{boot_id}/{epoch}-{index}.bin.gz`.
pub trait S3KeyFn: Send + Sync {
    /// Generate the S3 object key for the given segment.
    fn object_key(&self, segment: &SegmentInfo) -> String;
}

impl<F> S3KeyFn for F
where
    F: Fn(&SegmentInfo) -> String + Send + Sync,
{
    fn object_key(&self, segment: &SegmentInfo) -> String {
        self(segment)
    }
}

/// Configuration for S3 uploads.
///
/// Only `bucket` and `service_name` are required. The remaining builder fields
/// have sensible defaults:
///
/// - `instance_path`: system hostname
/// - `prefix`: none (keys start at the time bucket)
/// - `region`: auto-detected via `HeadBucket`
/// - `key_fn`: built-in time-first layout
///
/// # Default key layout
///
/// ```text
/// {prefix}/{YYYY-MM-DD}/{HHMM}/{service_name}/{instance_path}/{boot_id}/{epoch_secs}-{index}.bin.gz
/// ```
///
/// The `boot_id` segment disambiguates segment indices across process
/// restarts — without it, a service that restarts will produce colliding
/// `{epoch_secs}-{index}` names.
///
/// Override with [`key_fn`](S3ConfigBuilder::key_fn) for a custom layout.
#[derive(Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct S3Config {
    bucket: String,
    service_name: String,
    /// Instance identifier for S3 key paths. Defaults to the system hostname.
    #[builder(into, default = InstanceIdentity::from_hostname())]
    instance_path: InstanceIdentity,
    /// Identifies this process lifetime. Included as both S3 object metadata
    /// and in the default key path so segments (and segment indices) from
    /// different runs of the same service on the same host don't collide.
    ///
    /// Not a builder field: when telemetry is configured through the managed
    /// `Dial9Config` path the runtime injects the same
    /// boot_id it uses for the on-disk `{boot_id}/` namespace directory (via
    /// [`set_boot_id`](Self::set_boot_id)), so a local trace segment and its S3
    /// key share one identity. Defaults to a fresh `{4-alpha}-{pid}` when no
    /// namespace is in play.
    #[builder(skip = default_boot_id())]
    boot_id: String,
    /// Optional key prefix. When `None`, keys start at the time bucket.
    prefix: Option<String>,
    /// Optional AWS region override. When `None`, uses the SDK default.
    region: Option<String>,
    /// Custom S3 key function. When set, overrides the default key layout.
    #[builder(with = |key_fn: impl S3KeyFn + 'static| Arc::new(key_fn) as Arc<dyn S3KeyFn>)]
    key_fn: Option<Arc<dyn S3KeyFn>>,
    /// Per-attempt wall-clock timeout for individual S3 operations
    /// (`PutObject`, `HeadBucket`). Bounds how long a single HTTP attempt may
    /// stall before the SDK aborts it; the SDK retry policy and the pipeline
    /// circuit breaker then decide whether to re-drive. Without it a hung
    /// request could block the upload worker indefinitely. Defaults to 30s.
    ///
    /// Only applied to the client dial9 builds itself (the `.s3(..)` path). A
    /// client supplied through `.s3_with_client(..)` keeps its own timeout
    /// configuration untouched.
    #[builder(default = Duration::from_secs(30))]
    operation_attempt_timeout: Duration,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("service_name", &self.service_name)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .field("operation_attempt_timeout", &self.operation_attempt_timeout)
            .finish_non_exhaustive()
    }
}

impl S3Config {
    /// The S3 bucket name.
    pub(crate) fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Override the boot_id so S3 keys match the on-disk namespace directory.
    /// Called cross-crate by the runtime builder when the writer is namespaced.
    pub fn set_boot_id(&mut self, boot_id: impl Into<String>) {
        self.boot_id = boot_id.into();
    }

    /// The configured fields as `(key, value)` pairs, for attaching as S3
    /// object metadata or for inspection.
    pub fn as_metadata(&self) -> impl Iterator<Item = (&str, &str)> {
        [
            ("bucket", self.bucket.as_str()),
            ("service_name", self.service_name.as_str()),
            ("instance_path", self.instance_path.as_str()),
            ("boot_id", self.boot_id.as_str()),
        ]
        .into_iter()
        .chain(self.prefix.as_ref().map(|p| ("prefix", p.as_str())))
        .chain(self.region.as_ref().map(|r| ("region", r.as_str())))
    }

    /// Optional region override for the S3 client.
    pub(crate) fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    /// Per-attempt timeout applied to the client dial9 builds itself.
    pub(crate) fn operation_attempt_timeout(&self) -> Duration {
        self.operation_attempt_timeout
    }

    /// Build the S3 object key for a sealed segment.
    ///
    /// If a custom `key_fn` is set, delegates to it. Otherwise uses the
    /// default time-first layout:
    /// `{prefix}/{date}/{HHMM}/{service}/{instance}/{boot_id}/{epoch_secs}-{index}.bin.gz`
    pub(crate) fn object_key(
        &self,
        segment: &SegmentRef,
        metadata: &HashMap<String, String>,
    ) -> String {
        let epoch_secs: u64 = metadata
            .get("epoch_secs")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        if let Some(key_fn) = &self.key_fn {
            let info = SegmentInfo {
                index: segment.index(),
                epoch_secs,
                boot_id: self.boot_id.clone(),
            };
            return key_fn.object_key(&info);
        }
        let date_hour = time_bucket_from_epoch(epoch_secs);
        let ts = epoch_secs.to_string();

        let extension = if metadata
            .get("content_encoding")
            .is_some_and(|v| v == "gzip")
        {
            ".bin.gz"
        } else {
            ".bin"
        };

        let suffix = format!(
            "{}/{}/{}/{}/{}-{}{}",
            date_hour,
            self.service_name,
            self.instance_path.as_str(),
            self.boot_id,
            ts,
            segment.index(),
            extension,
        );
        match &self.prefix {
            Some(p) => format!("{p}/{suffix}"),
            None => suffix,
        }
    }

    /// Key of the per-dump manifest object: `{prefix}/dumps/{dump_id}.json`.
    pub(crate) fn manifest_key(&self, dump_id: &str) -> String {
        match &self.prefix {
            Some(p) => format!("{p}/dumps/{dump_id}.json"),
            None => format!("dumps/{dump_id}.json"),
        }
    }
}

/// JSON document written at `{prefix}/dumps/{dump_id}.json` when a dump
/// completes: the index answering "which trace objects belong to this
/// dump?" in a single GET. Its presence doubles as the cross-process
/// completion signal.
#[derive(Debug, serde::Serialize)]
pub(crate) struct DumpManifest {
    pub(crate) dump_id: String,
    pub(crate) triggered_at: String,
    pub(crate) time_range: [String; 2],
    pub(crate) segments_processed: usize,
    pub(crate) metadata: std::collections::BTreeMap<String, String>,
    pub(crate) segments: Vec<String>,
}

impl DumpManifest {
    pub(crate) fn new(
        completion: &dial9_core::dump::DumpCompletion,
        segments: Vec<String>,
    ) -> Self {
        Self {
            dump_id: completion.dump_id.to_string(),
            triggered_at: rfc3339(completion.triggered_at),
            time_range: [
                rfc3339(completion.time_range.0),
                rfc3339(completion.time_range.1),
            ],
            segments_processed: completion.segments_processed,
            metadata: completion
                .metadata
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            segments,
        }
    }
}

fn rfc3339(t: std::time::SystemTime) -> String {
    time::OffsetDateTime::from(t)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "invalid-timestamp".to_string())
}

/// S3 user-metadata keys ride HTTP headers; only pass caller keys that are
/// trivially valid and do not collide with the fixed per-object fields.
fn valid_user_metadata_key(key: &str) -> bool {
    const RESERVED: &[&str] = &[
        "service",
        "boot-id",
        "segment-index",
        "start-time",
        "host",
        "dump-id",
    ];
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && !RESERVED.contains(&key)
}

/// Values ride HTTP headers too; a non-ASCII or oversized value would fail
/// the whole trace-object PUT, so a bad caller pair is skipped instead.
fn valid_user_metadata_value(value: &str) -> bool {
    value.len() <= 256 && value.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// Convert epoch seconds to `YYYY-MM-DD/HHMM` string for S3 key bucketing.
fn time_bucket_from_epoch(epoch_secs: u64) -> String {
    let dt = time::OffsetDateTime::from_unix_timestamp(epoch_secs as i64)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}/{:02}{:02}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute()
    )
}

/// Gzip-compress a file synchronously. Intended for use with `spawn_blocking`.
#[cfg(test)]
pub(crate) fn gzip_compress_file_sync(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::{Read, Write};
    let mut file = std::fs::File::open(path)?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        encoder.write_all(&buf[..n])?;
    }
    encoder.finish()
}

/// Uploads sealed trace segments to S3.
pub(crate) struct S3Uploader {
    client: Client,
    config: S3Config,
}

impl std::fmt::Debug for S3Uploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Uploader").finish_non_exhaustive()
    }
}

impl S3Uploader {
    /// Create a new uploader with the given S3 client and config.
    pub(crate) fn new(client: Client, config: S3Config) -> Self {
        Self { client, config }
    }

    /// Upload segment bytes to S3, then delete the local file on success.
    ///
    /// Returns the S3 key of the uploaded object.
    pub(crate) async fn upload_and_delete(
        &self,
        segment: &SegmentRef,
        payload: dial9_core::pipeline::Payload,
        metadata: &HashMap<String, String>,
    ) -> Result<String, ProcessErrorKind> {
        let key = self.config.object_key(segment, metadata);

        let content_type = if metadata
            .get("content_encoding")
            .is_some_and(|v| v == "gzip")
        {
            "application/gzip"
        } else {
            "application/octet-stream"
        };

        let mut req = self
            .client
            .put_object()
            .bucket(&self.config.bucket)
            .key(&key)
            .content_type(content_type)
            .metadata("service", &self.config.service_name)
            .metadata("boot-id", &self.config.boot_id)
            .metadata("segment-index", segment.index().to_string())
            .metadata(
                "start-time",
                metadata
                    .get("epoch_secs")
                    .map(|s| s.as_str())
                    .unwrap_or("0"),
            )
            .metadata("host", self.config.instance_path.as_str());

        // Triggered dumps: tag the object with every dump it belongs to
        // (comma-joined), plus caller correlation pairs with the `dump.`
        // namespace stripped.
        if let Some(dump_ids) = metadata.get("dump_id") {
            req = req.metadata("dump-id", dump_ids);
            for (k, v) in metadata {
                if let Some(stripped) = k.strip_prefix("dump.") {
                    let header_key = stripped.to_ascii_lowercase();
                    if valid_user_metadata_key(&header_key) && valid_user_metadata_value(v) {
                        req = req.metadata(header_key, v);
                    } else {
                        rate_limited!(Duration::from_secs(60), {
                            tracing::warn!(
                                target: "dial9_worker",
                                key = %stripped,
                                "dump metadata pair not valid as S3 user metadata, skipping"
                            );
                        });
                    }
                }
            }
        }

        req.body(aws_sdk_s3::primitives::ByteStream::from(
            payload.into_bytes(),
        ))
        .send()
        .await
        .map_err(put_error_kind)?;

        // Remove local files if disk-backed (memory segments are gone once popped).
        if let Some(path) = segment.disk_path() {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(target: "dial9_worker", path = %path.display(), "segment already removed");
                }
                Err(e) => return Err(e.into()),
            }
        }

        Ok(key)
    }

    /// Key the manifest for `dump_id` would be written at.
    pub(crate) fn manifest_key(&self, dump_id: &str) -> String {
        self.config.manifest_key(dump_id)
    }

    /// PUT a dump manifest. Small JSON object, no local file involved.
    pub(crate) async fn upload_manifest(
        &self,
        key: &str,
        body: Vec<u8>,
    ) -> Result<(), ProcessErrorKind> {
        self.client
            .put_object()
            .bucket(&self.config.bucket)
            .key(key)
            .content_type("application/json")
            .body(aws_sdk_s3::primitives::ByteStream::from(body))
            .send()
            .await
            .map_err(put_error_kind)?;
        Ok(())
    }
}

// === S3 pipeline processor ===

/// S3 uploader processor. Construction is synchronous — the AWS client and
/// bucket region are resolved lazily on the first `process()` call, inside
/// the worker's tokio runtime.
pub struct S3PipelineUploader {
    state: S3UploaderState,
    /// Triggered mode: object keys written per dump id, accumulated while
    /// the dump is open and flushed into its manifest at `finalize_dump`.
    /// A key appears under several ids when forward windows overlap.
    /// `pub(crate)` so the finalize tests can seed and inspect it.
    pub(crate) dump_keys: HashMap<String, Vec<String>>,
}

enum S3UploaderState {
    Pending {
        s3_config: S3Config,
        client: Option<aws_sdk_s3::Client>,
    },
    Ready {
        uploader: S3Uploader,
        circuit_breaker: connection::CircuitBreaker,
    },
}

impl std::fmt::Debug for S3PipelineUploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3PipelineUploader").finish_non_exhaustive()
    }
}

impl S3PipelineUploader {
    /// Create a new uploader from an [`S3Config`](S3Config) and an
    /// optional pre-built S3 client. If `client` is `None`, the default
    /// AWS configuration chain is used. Region detection and transfer
    /// manager construction are deferred to the first `process()` call.
    pub fn new(s3_config: S3Config, client: Option<aws_sdk_s3::Client>) -> Self {
        Self {
            state: S3UploaderState::Pending { s3_config, client },
            dump_keys: HashMap::new(),
        }
    }

    /// Set (or override) the pre-built S3 client. Must be called before the
    /// uploader has been initialized (i.e. before the first segment has been
    /// processed);
    /// Note: the only caller is the builder, which runs before the
    /// worker is spawned, so reaching the `Ready` arm is a programmer error.
    pub fn set_client(&mut self, client: aws_sdk_s3::Client) {
        match &mut self.state {
            S3UploaderState::Pending { client: slot, .. } => *slot = Some(client),
            S3UploaderState::Ready { .. } => {
                unreachable!("set_client called after uploader initialization")
            }
        }
    }

    /// Take any previously-stashed client out of a `Pending` uploader so it
    /// can be carried into a replacement. Returns `None` once the uploader
    /// has been initialized.
    pub fn take_client(&mut self) -> Option<aws_sdk_s3::Client> {
        match &mut self.state {
            S3UploaderState::Pending { client, .. } => client.take(),
            S3UploaderState::Ready { .. } => None,
        }
    }

    /// Override the pending config's boot_id so S3 keys use the on-disk
    /// namespace identity. The builder calls this before the worker spawns, so
    /// the `Ready` arm is unreachable in practice; a no-op there keeps it safe.
    pub fn set_boot_id(&mut self, boot_id: impl Into<String>) {
        if let S3UploaderState::Pending { s3_config, .. } = &mut self.state {
            s3_config.set_boot_id(boot_id);
        }
    }

    /// Construct an uploader directly in the `Ready` state. Test-only —
    /// production code goes through [`new`](Self::new) and lazy init.
    #[cfg(test)]
    pub(crate) fn from_ready(
        uploader: S3Uploader,
        circuit_breaker: connection::CircuitBreaker,
    ) -> Self {
        Self {
            state: S3UploaderState::Ready {
                uploader,
                circuit_breaker,
            },
            dump_keys: HashMap::new(),
        }
    }

    async fn initialize(
        s3_config: S3Config,
        client: Option<aws_sdk_s3::Client>,
    ) -> (S3Uploader, connection::CircuitBreaker) {
        let bootstrap_client = match client {
            Some(c) => c,
            None => {
                // Bound each attempt so a hung PutObject/HeadBucket can't wedge
                // the upload worker; retries are left to the SDK policy and the
                // pipeline circuit breaker. Only the client we build ourselves
                // gets this — a caller-supplied client keeps its own config.
                let timeout_config = aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                    .operation_attempt_timeout(s3_config.operation_attempt_timeout())
                    .build();
                let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .timeout_config(timeout_config)
                    .load()
                    .await;
                aws_sdk_s3::Client::new(&sdk_config)
            }
        };

        let region = match s3_config.region() {
            Some(r) => r.to_owned(),
            None => detect_bucket_region(&bootstrap_client, s3_config.bucket()).await,
        };
        tracing::info!(target: "dial9_worker", bucket = %s3_config.bucket(), %region, "resolved bucket region");

        // Rebuild the client with the correct region.
        let corrected_conf = bootstrap_client
            .config()
            .to_builder()
            .region(aws_sdk_s3::config::Region::new(region))
            .build();
        let corrected_client = aws_sdk_s3::Client::from_conf(corrected_conf);

        (
            S3Uploader::new(corrected_client, s3_config),
            connection::CircuitBreaker::new(),
        )
    }
}

impl SegmentProcessor for S3PipelineUploader {
    fn name(&self) -> &'static str {
        "S3Upload"
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            // Lazy init: clone the config + client and run `initialize`
            // without mutating `self.state`. If the init future panics or
            // is cancelled mid-await, the worker's outer `catch_unwind`
            // recovers and `self.state` stays `Pending`, so the next
            // segment will retry. Mutating before the await would leave
            // the uploader stuck in a transient state forever.
            if let S3UploaderState::Pending { s3_config, client } = &self.state {
                let cfg = s3_config.clone();
                let cli = client.clone();
                let (uploader, circuit_breaker) = Self::initialize(cfg, cli).await;
                self.state = S3UploaderState::Ready {
                    uploader,
                    circuit_breaker,
                };
            }
            let S3UploaderState::Ready {
                uploader,
                circuit_breaker,
            } = &mut self.state
            else {
                // unreachable: we just transitioned above and the state
                // doesn't otherwise revert. Fall through with an error so
                // a future refactor doesn't silently break.
                return Err(ProcessError::io(
                    data,
                    std::io::Error::other("S3 uploader in unexpected state"),
                ));
            };
            if !circuit_breaker.should_attempt() {
                tracing::debug!(target: "dial9_worker", segment = %data.segment(), "circuit breaker open, skipping upload");
                return Err(ProcessError::new(
                    data,
                    ProcessErrorKind::transfer(Box::from("circuit breaker open"), true),
                ));
            }
            let payload = data.take_payload();
            match uploader
                .upload_and_delete(data.segment(), payload, data.metadata())
                .await
            {
                Ok(key) => {
                    circuit_breaker.on_success();
                    // Triggered dumps: remember the key under every dump id
                    // the segment belongs to, for that dump's manifest.
                    if let Some(dump_ids) = data.metadata().get("dump_id") {
                        for id in dump_ids.split(',').filter(|id| !id.is_empty()) {
                            self.dump_keys
                                .entry(id.to_string())
                                .or_default()
                                .push(key.clone());
                        }
                    }
                    rate_limited!(Duration::from_secs(10), {
                        tracing::info!(target: "dial9_worker", "uploaded {key}");
                    });
                    Ok(data)
                }
                Err(kind) => {
                    if kind.already_deleted() {
                        tracing::debug!(target: "dial9_worker", segment = %data.segment(), "segment already evicted, skipping");
                    } else {
                        circuit_breaker.on_failure();
                        rate_limited!(Duration::from_secs(60), {
                            tracing::warn!(target: "dial9_worker", error = %kind, "upload failed");
                        });
                    }
                    Err(ProcessError::new(data, kind))
                }
            }
        })
    }

    fn finalize_dump(
        &mut self,
        completion: &dial9_core::dump::DumpCompletion,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        // Always take the entry so per-dump state clears even when no
        // manifest gets written. An empty dump still gets an
        // (empty-segments) manifest: its presence is the cross-process
        // completion signal, so it is only written for dumps that
        // completed (a failed dump resolves `Err` and leaves no manifest).
        let segments = self
            .dump_keys
            .remove(&completion.dump_id.to_string())
            .unwrap_or_default();
        if completion.failed {
            return Box::pin(std::future::ready(None));
        }
        let manifest = DumpManifest::new(completion, segments);
        Box::pin(async move {
            // The manifest may be the first object of the run (e.g. an
            // empty dump before any segment upload): lazily initialize
            // exactly like `process()` does.
            if let S3UploaderState::Pending { s3_config, client } = &self.state {
                let cfg = s3_config.clone();
                let cli = client.clone();
                let (uploader, circuit_breaker) = Self::initialize(cfg, cli).await;
                self.state = S3UploaderState::Ready {
                    uploader,
                    circuit_breaker,
                };
            }
            let S3UploaderState::Ready {
                uploader,
                circuit_breaker,
            } = &mut self.state
            else {
                return None;
            };
            if !circuit_breaker.should_attempt() {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(target: "dial9_worker", dump_id = %manifest.dump_id, "circuit breaker open, skipping dump manifest");
                });
                return None;
            }
            let body = match serde_json::to_vec(&manifest) {
                Ok(body) => body,
                Err(e) => {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(target: "dial9_worker", error = %e, "failed to serialize dump manifest");
                    });
                    return None;
                }
            };
            let key = uploader.manifest_key(&manifest.dump_id);
            // Best-effort: a failed manifest PUT never fails the receipt.
            match uploader.upload_manifest(&key, body).await {
                Ok(()) => {
                    circuit_breaker.on_success();
                    Some(key)
                }
                Err(e) => {
                    circuit_breaker.on_failure();
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(target: "dial9_worker", error = %e, dump_id = %manifest.dump_id, "failed to write dump manifest");
                    });
                    None
                }
            }
        })
    }
}

/// Detect the region of an S3 bucket via HeadBucket.
async fn detect_bucket_region(client: &aws_sdk_s3::Client, bucket: &str) -> String {
    match client.head_bucket().bucket(bucket).send().await {
        Ok(resp) => {
            let region = resp.bucket_region().unwrap_or("us-east-1");
            if resp.bucket_region().is_none() {
                tracing::warn!(
                    target: "dial9_worker",
                    %bucket,
                    "HeadBucket succeeded but returned no region, falling back to us-east-1"
                );
            }
            region.to_owned()
        }
        Err(e) => {
            let from_header = e
                .raw_response()
                .and_then(|r| r.headers().get("x-amz-bucket-region"))
                .map(|v| v.to_owned());
            match from_header {
                Some(r) => r,
                None => {
                    tracing::warn!(
                        target: "dial9_worker",
                        %bucket,
                        error = ?e,
                        "failed to detect bucket region, falling back to us-east-1"
                    );
                    "us-east-1".to_owned()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use dial9_core::pipeline::Payload;
    use dial9_core::pipeline::SegmentRef;
    use flate2::read::GzDecoder;
    use std::io::Read;
    use std::path::PathBuf;

    fn gzip_compress_bytes(data: &[u8]) -> std::io::Result<Vec<u8>> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(data)?;
        encoder.finish()
    }

    /// Build a config with a fixed boot_id for deterministic key assertions.
    fn with_boot_id(mut config: S3Config, boot_id: &str) -> S3Config {
        config.set_boot_id(boot_id);
        config
    }

    fn make_config() -> S3Config {
        with_boot_id(
            S3Config::builder()
                .bucket("test-bucket")
                .prefix("traces")
                .service_name("checkout-api")
                .instance_path("us-east-1/i-0abc123")
                .build(),
            "test-boot-id",
        )
    }

    fn make_segment(path: impl Into<PathBuf>, index: u32) -> SegmentRef {
        dial9_core::test_util::disk_segment(path, index)
    }

    fn make_metadata(epoch_secs: u64) -> HashMap<String, String> {
        HashMap::from([
            ("epoch_secs".into(), epoch_secs.to_string()),
            ("content_encoding".into(), "gzip".into()),
        ])
    }

    /// Create an `aws_sdk_s3::Client` backed by s3s-fs (in-memory fake S3).
    /// The same builder drives both the uploader under test and the read-back
    /// client tests use to verify uploaded objects; each call returns an
    /// independent client over the same on-disk bucket at `fs_root`.
    fn fake_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
        let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
        let mut builder = s3s::service::S3ServiceBuilder::new(fs);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_service = builder.build();
        let s3_client: s3s_aws::Client = s3_service.into();

        let s3_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(s3_client)
            .force_path_style(true)
            .build();

        aws_sdk_s3::Client::from_conf(s3_config)
    }

    #[test]
    fn operation_attempt_timeout_defaults_to_30s_and_is_overridable() {
        let default_cfg = S3Config::builder().bucket("b").service_name("svc").build();
        check!(
            default_cfg.operation_attempt_timeout() == std::time::Duration::from_secs(30),
            "self-built client must get a bounded per-attempt timeout by default"
        );

        let custom = S3Config::builder()
            .bucket("b")
            .service_name("svc")
            .operation_attempt_timeout(std::time::Duration::from_secs(5))
            .build();
        check!(custom.operation_attempt_timeout() == std::time::Duration::from_secs(5));
    }

    // --- Key format tests ---

    #[test]
    fn object_key_includes_all_components() {
        let config = make_config();
        let segment = make_segment("/tmp/trace.3.bin", 3);
        let metadata = make_metadata(1741209000);
        let key = config.object_key(&segment, &metadata);
        check!(
            key == "traces/2025-03-05/2110/checkout-api/us-east-1/i-0abc123/test-boot-id/1741209000-3.bin.gz"
        );
    }

    #[test]
    fn object_key_empty_prefix() {
        let config = with_boot_id(
            S3Config::builder()
                .bucket("my-traces")
                .service_name("checkout-api")
                .instance_path("us-east-1/i-0abc123")
                .build(),
            "test-boot-id",
        );
        let segment = make_segment("/tmp/trace.0.bin", 0);
        let metadata = make_metadata(1741209000);
        let key = config.object_key(&segment, &metadata);
        check!(
            key == "2025-03-05/2110/checkout-api/us-east-1/i-0abc123/test-boot-id/1741209000-0.bin.gz"
        );
    }

    #[test]
    fn object_key_without_compression() {
        let config = make_config();
        let segment = make_segment("/tmp/trace.0.bin", 0);
        let metadata = HashMap::from([("epoch_secs".into(), "1741209000".into())]);
        let key = config.object_key(&segment, &metadata);
        check!(
            key == "traces/2025-03-05/2110/checkout-api/us-east-1/i-0abc123/test-boot-id/1741209000-0.bin"
        );
    }

    #[test]
    fn default_boot_id_is_alpha_timestamp_and_pid() {
        let id = default_boot_id();
        let (ts, pid) = id.split_once("-").unwrap();
        assert_eq!(ts.len(), 4);
        pid.parse::<u64>().unwrap();
    }

    #[test]
    fn custom_key_fn_overrides_default() {
        let config = S3Config::builder()
            .bucket("test-bucket")
            .service_name("svc")
            .instance_path("host")
            .key_fn(|segment: &SegmentInfo| {
                format!("custom/{}-{}.bin.gz", segment.epoch_secs, segment.index)
            })
            .build();
        let segment = make_segment("/tmp/trace.5.bin", 5);
        let metadata = make_metadata(1741209000);
        let key = config.object_key(&segment, &metadata);
        check!(key == "custom/1741209000-5.bin.gz");
    }

    // --- Gzip compression tests ---

    #[test]
    fn gzip_compress_roundtrips() {
        let original = b"hello world, this is trace data that should compress well!";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        std::fs::write(&path, original).unwrap();

        let compressed = gzip_compress_file_sync(&path).unwrap();
        check!(compressed[..] != original[..]);

        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        check!(decompressed == original);
    }

    #[test]
    fn gzip_compress_bytes_roundtrips() {
        let original = b"hello world, this is trace data that should compress well!";
        let compressed = gzip_compress_bytes(original).unwrap();
        check!(compressed[..] != original[..]);

        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        check!(decompressed == original);
    }

    #[test]
    fn gzip_compress_empty_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, b"").unwrap();

        let compressed = gzip_compress_file_sync(&path).unwrap();
        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        check!(decompressed.is_empty());
    }

    // --- Builder tests ---

    #[test]
    fn builder_prefix_defaults_to_empty() {
        let config = S3Config::builder()
            .bucket("bucket")
            .service_name("svc")
            .instance_path("path")
            .build();
        let segment = make_segment("/tmp/trace.0.bin", 0);
        let metadata = make_metadata(1741209000);
        let key = config.object_key(&segment, &metadata);
        // No prefix → date-hour is first component
        check!(key.starts_with("2025-03-05/"));
    }

    // --- S3 integration tests via s3s-fs ---

    #[tokio::test]
    async fn upload_and_delete_writes_to_s3_and_removes_local_file() {
        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();

        // Create the bucket directory (s3s-fs uses directories as buckets)
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let client = fake_s3_client(s3_root.path());
        let raw_client = fake_s3_client(s3_root.path());
        let config = make_config();
        let uploader = S3Uploader::new(client, config);

        // Write a fake segment file
        let segment_path = local_dir.path().join("trace.0.bin");
        let original_data = b"trace data here";
        std::fs::write(&segment_path, original_data).unwrap();
        let segment = make_segment(&segment_path, 0);

        // Compress, then upload and delete
        let compressed = gzip_compress_file_sync(&segment_path).unwrap();
        let metadata = make_metadata(1741209000);
        let key = uploader
            .upload_and_delete(&segment, Payload::from_vec(compressed), &metadata)
            .await
            .unwrap();

        check!(
            key == "traces/2025-03-05/2110/checkout-api/us-east-1/i-0abc123/test-boot-id/1741209000-0.bin.gz"
        );

        // Local file should be deleted
        check!(!segment_path.exists());

        // Download from S3 and verify contents
        let resp = raw_client
            .get_object()
            .bucket("test-bucket")
            .key(&key)
            .send()
            .await
            .unwrap();
        let body = resp.body.collect().await.unwrap().into_bytes();
        let mut decoder = GzDecoder::new(&body[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        check!(decompressed == original_data);
    }

    #[tokio::test]
    async fn uploaded_object_contains_gzipped_original_data() {
        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let client = fake_s3_client(s3_root.path());
        let raw_s3_client = fake_s3_client(s3_root.path());

        let config = make_config();
        let uploader = S3Uploader::new(client, config);

        let original_data = b"important trace data that must survive the roundtrip";
        let segment_path = local_dir.path().join("trace.5.bin");
        std::fs::write(&segment_path, original_data).unwrap();
        let segment = make_segment(&segment_path, 5);

        let compressed = gzip_compress_file_sync(&segment_path).unwrap();
        let metadata = make_metadata(1741209000);
        let _key = uploader
            .upload_and_delete(&segment, Payload::from_vec(compressed), &metadata)
            .await
            .unwrap();

        // Read back from fake S3
        let get_result = raw_s3_client
            .get_object()
            .bucket("test-bucket")
            .key(&_key)
            .send()
            .await
            .unwrap();

        let body = get_result.body.collect().await.unwrap().into_bytes();

        // Body should be gzip — decompress and verify
        let mut decoder = GzDecoder::new(&body[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();
        check!(decompressed == original_data);
    }

    #[tokio::test]
    async fn upload_sets_s3_object_metadata_headers() {
        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let client = fake_s3_client(s3_root.path());
        let raw_s3_client = fake_s3_client(s3_root.path());

        let config = with_boot_id(
            S3Config::builder()
                .bucket("test-bucket")
                .prefix("traces")
                .service_name("checkout-api")
                .instance_path("us-east-1/i-0abc123")
                .build(),
            "a3f7c2d1-dead-beef-1234-567890abcdef",
        );
        let uploader = S3Uploader::new(client, config);

        let segment_path = local_dir.path().join("trace.3.bin");
        std::fs::write(&segment_path, b"trace data").unwrap();
        let segment = make_segment(&segment_path, 3);

        let compressed = gzip_compress_file_sync(&segment_path).unwrap();
        let metadata = make_metadata(1741209000);
        let key = uploader
            .upload_and_delete(&segment, Payload::from_vec(compressed), &metadata)
            .await
            .unwrap();

        // HeadObject to read back metadata
        let head = raw_s3_client
            .head_object()
            .bucket("test-bucket")
            .key(&key)
            .send()
            .await
            .unwrap();

        let meta = head.metadata().unwrap();
        check!(meta.get("service").unwrap() == "checkout-api");
        check!(meta.get("boot-id").unwrap() == "a3f7c2d1-dead-beef-1234-567890abcdef");
        check!(meta.get("segment-index").unwrap() == "3");
        check!(meta.get("start-time").unwrap() == "1741209000");
        check!(meta.get("host").unwrap() == "us-east-1/i-0abc123");
        // No dump tagging in continuous mode.
        check!(!meta.contains_key("dump-id"));
    }

    #[tokio::test]
    async fn upload_attaches_dump_id_and_stripped_dump_pairs() {
        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let client = fake_s3_client(s3_root.path());
        let raw_s3_client = fake_s3_client(s3_root.path());
        let uploader = S3Uploader::new(client, make_config());

        let segment_path = local_dir.path().join("trace.4.bin");
        std::fs::write(&segment_path, b"trace data").unwrap();
        let segment = make_segment(&segment_path, 4);

        let mut metadata = make_metadata(1741209000);
        metadata.insert("dump_id".into(), "01ABC,01DEF".into());
        metadata.insert("dump.reason".into(), "idle-ratio-drop".into());
        metadata.insert("dump.Incident ID!".into(), "i-99".into()); // invalid key: skipped
        metadata.insert("dump.host".into(), "spoofed".into()); // reserved: skipped
        metadata.insert("dump.note".into(), "caf\u{e9}".into()); // non-ASCII value: skipped

        let compressed = gzip_compress_file_sync(&segment_path).unwrap();
        let key = uploader
            .upload_and_delete(&segment, Payload::from_vec(compressed), &metadata)
            .await
            .unwrap();

        let head = raw_s3_client
            .head_object()
            .bucket("test-bucket")
            .key(&key)
            .send()
            .await
            .unwrap();
        let meta = head.metadata().unwrap();
        check!(meta.get("dump-id").unwrap() == "01ABC,01DEF");
        check!(meta.get("reason").unwrap() == "idle-ratio-drop");
        check!(!meta.contains_key("incident id!"));
        check!(!meta.contains_key("note"), "non-ASCII value skipped");
        // Reserved fixed field never overridden by caller pairs.
        check!(meta.get("host").unwrap() == "us-east-1/i-0abc123");
    }

    #[test]
    fn manifest_key_layout() {
        let with_prefix = make_config();
        check!(with_prefix.manifest_key("01ABC") == "traces/dumps/01ABC.json");

        let no_prefix = S3Config::builder()
            .bucket("b")
            .service_name("s")
            .instance_path("i")
            .build();
        check!(no_prefix.manifest_key("01ABC") == "dumps/01ABC.json");
    }

    #[test]
    fn dump_manifest_serializes_doc_shape() {
        use std::time::{Duration, UNIX_EPOCH};

        let dump_id = dial9_core::test_util::new_dump_id();
        let completion = dial9_core::test_util::new_dump_completion(
            dump_id,
            UNIX_EPOCH + Duration::from_secs(1741209000),
            (
                UNIX_EPOCH + Duration::from_secs(1741208700),
                UNIX_EPOCH + Duration::from_secs(1741209300),
            ),
            2,
            vec![("reason".into(), "idle-ratio-drop".into())],
            false,
        );
        let manifest = DumpManifest::new(
            &completion,
            vec!["traces/a.bin.gz".into(), "traces/b.bin.gz".into()],
        );
        let value = serde_json::to_value(&manifest).unwrap();

        check!(value["dump_id"] == serde_json::json!(dump_id.to_string()));
        check!(value["triggered_at"] == serde_json::json!("2025-03-05T21:10:00Z"));
        check!(
            value["time_range"]
                == serde_json::json!(["2025-03-05T21:05:00Z", "2025-03-05T21:15:00Z"])
        );
        check!(value["segments_processed"] == serde_json::json!(2));
        check!(value["metadata"] == serde_json::json!({"reason": "idle-ratio-drop"}));
        check!(value["segments"] == serde_json::json!(["traces/a.bin.gz", "traces/b.bin.gz"]));
    }

    #[tokio::test]
    async fn upload_failure_does_not_delete_local_file() {
        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let client = fake_s3_client(s3_root.path());
        let config = make_config();
        let uploader = S3Uploader::new(client, config);

        let segment_path = local_dir.path().join("trace.0.bin");
        std::fs::write(&segment_path, b"should survive").unwrap();

        let segment = make_segment(&segment_path, 0);
        let compressed = gzip_compress_bytes(b"should survive").unwrap();
        let metadata = make_metadata(1741209000);

        // Destroy the S3 backend filesystem — uploads will fail
        drop(s3_root);

        let result = uploader
            .upload_and_delete(&segment, Payload::from_vec(compressed), &metadata)
            .await;

        check!(result.is_err());
        // The local file must survive the failed upload
        check!(segment_path.exists());
    }

    // --- Review finding #6: object_key with epoch_secs fallback to 0 ---

    #[test]
    fn object_key_epoch_secs_fallback_to_zero_produces_1970_path() {
        let config = make_config();
        let segment = make_segment("/tmp/trace.0.bin", 0);
        // No epoch_secs in metadata — falls back to 0
        let metadata = HashMap::new();
        let key = config.object_key(&segment, &metadata);
        // epoch 0 → 1970-01-01/0000 — this is a silent misconfiguration
        check!(key.contains("1970-01-01/0000"));
    }

    #[test]
    fn object_key_epoch_secs_unparseable_falls_back_to_zero() {
        let config = make_config();
        let segment = make_segment("/tmp/trace.0.bin", 0);
        let metadata = HashMap::from([("epoch_secs".into(), "not-a-number".into())]);
        let key = config.object_key(&segment, &metadata);
        check!(key.contains("1970-01-01/0000"));
    }
}

/// Worker-integration tests: drive a real `S3PipelineUploader` through the
/// pipeline against an s3s-fs fake S3, via the `dial9_core::test_util`
/// helpers (the worker internals are not exposed cross-crate).
#[cfg(test)]
mod worker_integration_tests {
    use super::{S3PipelineUploader, S3Uploader};
    use crate::connection::CircuitBreaker;
    use crate::s3;
    use assert2::check;
    use dial9_core::pipeline::SegmentProcessor;
    use dial9_core::worker::processors::GzipCompressor;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    // === Unit tests (no worker) ===

    /// A NotFound read (an evicted segment) must not degrade the circuit
    /// breaker: only a genuine transfer error opens it.
    #[tokio::test]
    async fn evicted_file_does_not_trip_circuit_breaker() {
        let dir = tempfile::tempdir().unwrap();
        // A path that does not exist on disk (simulates eviction).
        let missing = dir.path().join("trace.0.bin");

        let mut cb = CircuitBreaker::new();
        // Mirror the upload skip logic: a NotFound read is skipped, not a
        // failure; any other error degrades the breaker.
        if cb.should_attempt() {
            match tokio::fs::read(&missing).await {
                Ok(_) => cb.on_success(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => cb.on_failure(),
            }
        }

        check!(cb == CircuitBreaker::Closed);
    }

    /// `set_client` is only valid while the uploader is `Pending`. Calling it
    /// on a `Ready` uploader indicates internal misuse and must panic rather
    /// than silently drop the new client.
    #[test]
    #[should_panic(expected = "set_client called after uploader initialization")]
    fn set_client_after_ready_panics() {
        let s3_config = s3::S3Config::builder()
            .bucket("test")
            .service_name("test")
            .instance_path("test")
            .region("us-east-1")
            .build();
        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        let sdk_client = aws_sdk_s3::Client::from_conf(sdk_config);
        let uploader = S3Uploader::new(sdk_client.clone(), s3_config);
        let mut pipeline_uploader = S3PipelineUploader::from_ready(uploader, CircuitBreaker::new());
        pipeline_uploader.set_client(sdk_client);
    }

    /// The S3 stage clears per-dump state but writes no manifest for a
    /// failed dump (manifest presence means successful completion).
    #[tokio::test]
    async fn s3_finalize_skips_manifest_for_failed_dump() {
        let config = s3::S3Config::builder()
            .bucket("b")
            .service_name("s")
            .instance_path("i")
            .build();
        let mut uploader = S3PipelineUploader::new(config, None);

        let dump_id = dial9_core::test_util::new_dump_id();
        uploader
            .dump_keys
            .insert(dump_id.to_string(), vec!["traces/x.bin.gz".into()]);

        let now = std::time::SystemTime::now();
        let completion = dial9_core::test_util::new_dump_completion(
            dump_id,
            now,
            (now, now),
            0,
            Vec::new(),
            true,
        );
        let key = uploader.finalize_dump(&completion).await;
        check!(key.is_none());
        check!(
            uploader.dump_keys.is_empty(),
            "per-dump state still cleared"
        );
    }

    // === Worker-integration tests (real S3 via s3s-fs, driven through dial9_core::test_util) ===

    fn fake_s3_client(fs_root: &Path) -> aws_sdk_s3::Client {
        let fs = s3s_fs::FileSystem::new(fs_root).unwrap();
        let mut builder = s3s::service::S3ServiceBuilder::new(fs);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_service = builder.build();
        let s3_client: s3s_aws::Client = s3_service.into();
        let s3_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(s3_client)
            .force_path_style(true)
            .build();
        aws_sdk_s3::Client::from_conf(s3_config)
    }

    fn s3_uploader_for(root: &Path) -> S3PipelineUploader {
        let config = s3::S3Config::builder()
            .bucket("test-bucket")
            .prefix("traces")
            .service_name("test")
            .instance_path("test")
            .region("us-east-1")
            .build();
        let uploader = S3Uploader::new(fake_s3_client(root), config);
        S3PipelineUploader::from_ready(uploader, CircuitBreaker::new())
    }

    fn read_manifest(root: &Path, key: &str) -> serde_json::Value {
        let path = root.join("test-bucket").join(key);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("manifest at {} unreadable: {e}", path.display()));
        serde_json::from_slice(&bytes).unwrap()
    }

    fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[tokio::test]
    async fn dump_writes_manifest_listing_uploaded_keys() {
        let s3_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let pipeline = dial9_core::test_util::spawn_triggered_pipeline(vec![
            Box::new(GzipCompressor),
            Box::new(s3_uploader_for(s3_root.path())),
        ]);
        pipeline.seal(0, now_epoch());
        pipeline.seal(1, now_epoch());

        let receipt = pipeline
            .trigger
            .dump_current_data()
            .with_metadata("reason", "test")
            .await
            .unwrap();

        let manifest_key = receipt
            .manifest_key
            .clone()
            .expect("S3 pipeline writes a manifest");
        check!(
            manifest_key == format!("traces/dumps/{}.json", receipt.dump_id),
            "manifest key layout"
        );

        let manifest = read_manifest(s3_root.path(), &manifest_key);
        check!(manifest["dump_id"] == serde_json::json!(receipt.dump_id.to_string()));
        check!(manifest["segments_processed"] == serde_json::json!(2));
        check!(manifest["metadata"]["reason"] == serde_json::json!("test"));
        let segments = manifest["segments"].as_array().unwrap();
        check!(segments.len() == 2);
        for key in segments {
            let key = key.as_str().unwrap();
            check!(
                s3_root.path().join("test-bucket").join(key).exists(),
                "manifest lists a real object: {key}"
            );
        }

        pipeline.shutdown().await;
    }

    #[tokio::test]
    async fn overlapping_dumps_fan_out_shared_key_to_both_manifests() {
        let s3_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let pipeline = dial9_core::test_util::spawn_triggered_pipeline(vec![
            Box::new(GzipCompressor),
            Box::new(s3_uploader_for(s3_root.path())),
        ]);

        let fut_a = std::future::IntoFuture::into_future(
            pipeline
                .trigger
                .dump_time_range(Duration::from_secs(60), Duration::from_secs(1)),
        );
        let fut_b = std::future::IntoFuture::into_future(
            pipeline
                .trigger
                .dump_time_range(Duration::from_secs(60), Duration::from_secs(1)),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        pipeline.seal(0, now_epoch());

        let (receipt_a, receipt_b) = tokio::join!(fut_a, fut_b);
        let receipt_a = receipt_a.unwrap();
        let receipt_b = receipt_b.unwrap();

        let manifest_a = read_manifest(s3_root.path(), receipt_a.manifest_key.as_ref().unwrap());
        let manifest_b = read_manifest(s3_root.path(), receipt_b.manifest_key.as_ref().unwrap());
        let segs_a = manifest_a["segments"].as_array().unwrap();
        let segs_b = manifest_b["segments"].as_array().unwrap();
        check!(segs_a.len() == 1);
        check!(segs_a == segs_b, "the shared key appears in both manifests");

        pipeline.shutdown().await;
    }

    #[tokio::test]
    async fn empty_dump_still_writes_manifest_as_completion_signal() {
        let s3_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let pipeline = dial9_core::test_util::spawn_triggered_pipeline(vec![
            Box::new(GzipCompressor),
            Box::new(s3_uploader_for(s3_root.path())),
        ]);

        let receipt = pipeline.trigger.dump_current_data().await.unwrap();
        check!(receipt.segments_processed == 0);
        let manifest = read_manifest(s3_root.path(), receipt.manifest_key.as_ref().unwrap());
        check!(manifest["segments"] == serde_json::json!([]));

        pipeline.shutdown().await;
    }

    // === Flaky-retry end-to-end recovery ===

    /// s3s wrapper that fails the first `fail_n` writes with 500, then
    /// delegates to the inner backend.
    struct FlakyS3<S> {
        inner: S,
        remaining_failures: Arc<std::sync::atomic::AtomicU32>,
    }

    impl<S> FlakyS3<S> {
        fn should_fail(&self) -> bool {
            let prev = self.remaining_failures.load(Ordering::SeqCst);
            if prev == 0 {
                return false;
            }
            self.remaining_failures
                .compare_exchange(prev, prev - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        }
    }

    #[async_trait::async_trait]
    impl<S: s3s::S3 + Send + Sync> s3s::S3 for FlakyS3<S> {
        async fn put_object(
            &self,
            req: s3s::S3Request<s3s::dto::PutObjectInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
            if self.should_fail() {
                return Err(s3s::S3Error::with_message(
                    s3s::S3ErrorCode::InternalError,
                    "injected 500",
                ));
            }
            self.inner.put_object(req).await
        }
    }

    struct FlakyHarness {
        uploader: S3Uploader,
        fail_counter: Arc<std::sync::atomic::AtomicU32>,
        s3_root: tempfile::TempDir,
    }

    /// Read the single object out of the fake S3 bucket. Panics if there
    /// isn't exactly one. Used to assert uploaded bytes survived retries.
    fn read_only_object(s3_root: &Path) -> Vec<u8> {
        fn walk(p: &Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(p) else { return };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.is_file()
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| !n.ends_with(".s3s-fs"))
                {
                    out.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(&s3_root.join("test-bucket"), &mut found);
        assert_eq!(found.len(), 1, "expected exactly one object, got {found:?}");
        std::fs::read(&found[0]).unwrap()
    }

    fn flaky_s3_harness(fail_n: u32) -> FlakyHarness {
        let s3_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();
        let fail_counter = Arc::new(std::sync::atomic::AtomicU32::new(fail_n));

        let fs = s3s_fs::FileSystem::new(s3_root.path()).unwrap();
        let flaky = FlakyS3 {
            inner: fs,
            remaining_failures: Arc::clone(&fail_counter),
        };
        let mut svc = s3s::service::S3ServiceBuilder::new(flaky);
        svc.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_client: s3s_aws::Client = svc.build().into();

        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            // Disable SDK-internal retries so each worker attempt = 1 PUT.
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
            .http_client(s3_client)
            .force_path_style(true)
            .build();
        let sdk_client = aws_sdk_s3::Client::from_conf(sdk_config);
        let s3_config = s3::S3Config::builder()
            .bucket("test-bucket")
            .service_name("test")
            .instance_path("test")
            .region("us-east-1")
            .build();
        FlakyHarness {
            uploader: S3Uploader::new(sdk_client, s3_config),
            fail_counter,
            s3_root,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mem_e2e_real_s3_pipeline_recovers_within_budget() {
        let FlakyHarness {
            uploader,
            fail_counter,
            s3_root,
        } = flaky_s3_harness(2);
        // > CB initial backoff (1s) so CB reopens between retries. CB
        // doubles per failure; budget=3 fits 1s+2s within the 15s cap below.
        let poll_interval = Duration::from_millis(1100);

        let payload = b"segment-payload-bytes".to_vec();
        let uploader_stage = S3PipelineUploader::from_ready(uploader, CircuitBreaker::new());

        tokio::time::timeout(
            Duration::from_secs(15),
            dial9_core::test_util::run_pipeline_continuous(
                vec![payload.clone()],
                vec![Box::new(uploader_stage)],
                poll_interval,
            ),
        )
        .await
        .expect("worker hung")
        .expect("pipeline run failed");

        check!(
            fail_counter.load(Ordering::SeqCst) == 0,
            "all injected failures consumed",
        );
        let uploaded = read_only_object(s3_root.path());
        check!(
            uploaded == payload,
            "uploaded body must match seal'd bytes (snapshot survived retries)",
        );
    }
}
