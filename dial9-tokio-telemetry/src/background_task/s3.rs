//! S3 uploader for sealed trace segments.
//!
//! Uploads processed segment bytes to S3 using the transfer manager.
//! Deletes local files only after confirmed upload.

use crate::background_task::ProcessErrorKind;
use crate::background_task::instance_metadata::InstanceIdentity;
use crate::background_task::sealed::SegmentRef;
use crate::rate_limit::rate_limited;
use aws_sdk_s3_transfer_manager::Client;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::background_task::boot_id::generate_boot_id as default_boot_id;

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
/// Only `bucket` and `service_name` are required. The remaining fields have
/// sensible defaults:
///
/// - `instance_path`: system hostname
/// - `boot_id`: `{4 alpha derived from timestamp}-{pid}` (unique per process start)
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
    /// Identifies this process lifetime. Included as both S3 object
    /// metadata and in the default key path. A new value each application
    /// start disambiguates segments (and segment indices) from different
    /// runs of the same service on the same host.
    ///
    /// Defaults to 4 random lowercase alpha characters.
    #[builder(default = default_boot_id())]
    boot_id: String,
    /// Optional key prefix. When `None`, keys start at the time bucket.
    prefix: Option<String>,
    /// Optional AWS region override. When `None`, uses the SDK default.
    region: Option<String>,
    /// Custom S3 key function. When set, overrides the default key layout.
    #[builder(with = |key_fn: impl S3KeyFn + 'static| Arc::new(key_fn) as Arc<dyn S3KeyFn>)]
    key_fn: Option<Arc<dyn S3KeyFn>>,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("service_name", &self.service_name)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl S3Config {
    /// The S3 bucket name.
    pub(crate) fn bucket(&self) -> &str {
        &self.bucket
    }

    pub(crate) fn as_metadata(&self) -> impl Iterator<Item = (&str, &str)> {
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
    pub(crate) fn new(completion: &crate::dump::DumpCompletion, segments: Vec<String>) -> Self {
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
pub struct S3Uploader {
    client: Client,
    config: S3Config,
}

impl std::fmt::Debug for S3Uploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Uploader").finish_non_exhaustive()
    }
}

impl S3Uploader {
    /// Create a new uploader with the given transfer manager client and config.
    pub fn new(client: Client, config: S3Config) -> Self {
        Self { client, config }
    }

    /// Upload segment bytes to S3, then delete the local file on success.
    ///
    /// Returns the S3 key of the uploaded object.
    pub(crate) async fn upload_and_delete(
        &self,
        segment: &SegmentRef,
        payload: super::Payload,
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

        let mut input = aws_sdk_s3_transfer_manager::operation::upload::UploadInput::builder()
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
            input = input.metadata("dump-id", dump_ids);
            for (k, v) in metadata {
                if let Some(stripped) = k.strip_prefix("dump.") {
                    let header_key = stripped.to_ascii_lowercase();
                    if valid_user_metadata_key(&header_key) && valid_user_metadata_value(v) {
                        input = input.metadata(header_key, v);
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

        let handle = input
            .body(payload.into_bytes().into())
            .initiate_with(&self.client)?;

        handle.join().await?;

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
        let handle = aws_sdk_s3_transfer_manager::operation::upload::UploadInput::builder()
            .bucket(&self.config.bucket)
            .key(key)
            .content_type("application/json")
            .body(bytes::Bytes::from(body).into())
            .initiate_with(&self.client)?;
        handle.join().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::Payload;
    use super::*;
    use crate::background_task::sealed::{SealedSegment, SegmentRef};
    use assert2::check;
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

    fn make_config() -> S3Config {
        S3Config::builder()
            .bucket("test-bucket")
            .prefix("traces")
            .service_name("checkout-api")
            .instance_path("us-east-1/i-0abc123")
            .boot_id("test-boot-id")
            .build()
    }

    fn make_segment(path: impl Into<PathBuf>, index: u32) -> SegmentRef {
        SegmentRef::Disk(SealedSegment {
            path: path.into(),
            index,
        })
    }

    fn make_metadata(epoch_secs: u64) -> HashMap<String, String> {
        HashMap::from([
            ("epoch_secs".into(), epoch_secs.to_string()),
            ("content_encoding".into(), "gzip".into()),
        ])
    }

    /// Create a transfer manager Client backed by s3s-fs (in-memory fake S3).
    fn fake_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3_transfer_manager::Client {
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

        let sdk_client = aws_sdk_s3::Client::from_conf(s3_config);

        let tm_config = aws_sdk_s3_transfer_manager::Config::builder()
            .client(sdk_client)
            .build();

        aws_sdk_s3_transfer_manager::Client::new(tm_config)
    }

    /// Create a raw aws_sdk_s3::Client for reading back objects from the fake S3.
    fn fake_raw_s3_client(fs_root: &std::path::Path) -> aws_sdk_s3::Client {
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
        let config = S3Config::builder()
            .bucket("my-traces")
            .service_name("checkout-api")
            .instance_path("us-east-1/i-0abc123")
            .boot_id("test-boot-id")
            .build();
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
            .boot_id("bid")
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
            .boot_id("bid")
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
        let raw_client = fake_raw_s3_client(s3_root.path());
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
        let raw_s3_client = fake_raw_s3_client(s3_root.path());

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
        let raw_s3_client = fake_raw_s3_client(s3_root.path());

        let config = S3Config::builder()
            .bucket("test-bucket")
            .prefix("traces")
            .service_name("checkout-api")
            .instance_path("us-east-1/i-0abc123")
            .boot_id("a3f7c2d1-dead-beef-1234-567890abcdef")
            .build();
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
        let raw_s3_client = fake_raw_s3_client(s3_root.path());
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
            .boot_id("boot")
            .build();
        check!(no_prefix.manifest_key("01ABC") == "dumps/01ABC.json");
    }

    #[test]
    fn dump_manifest_serializes_doc_shape() {
        use std::time::{Duration, UNIX_EPOCH};

        let (trigger, mut rx) = crate::dump::channel();
        trigger
            .dump_current_data()
            .with_metadata("reason", "idle-ratio-drop");
        let req = rx.rx.try_recv().unwrap();

        let completion = crate::dump::DumpCompletion {
            dump_id: req.id,
            triggered_at: UNIX_EPOCH + Duration::from_secs(1741209000),
            time_range: (
                UNIX_EPOCH + Duration::from_secs(1741208700),
                UNIX_EPOCH + Duration::from_secs(1741209300),
            ),
            segments_processed: 2,
            metadata: req.metadata,
            failed: false,
        };
        let manifest = DumpManifest::new(
            &completion,
            vec!["traces/a.bin.gz".into(), "traces/b.bin.gz".into()],
        );
        let value = serde_json::to_value(&manifest).unwrap();

        check!(value["dump_id"] == serde_json::json!(req.id.to_string()));
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
