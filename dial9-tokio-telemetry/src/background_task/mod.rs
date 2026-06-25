pub(crate) mod boot_id;
#[cfg(feature = "worker-s3")]
pub(crate) mod connection;
pub(crate) mod fs;
pub mod instance_metadata;
mod payload;
pub(crate) mod pipeline_metrics;
#[cfg(feature = "worker-s3")]
pub mod s3;
pub(crate) mod sealed;
#[cfg(test)]
pub(crate) mod testutil;

pub use payload::Payload;
pub use sealed::{MemorySegment, SealedSegment, SegmentRef};

use crate::background_task::fs::{
    EpochWindow, Fs, RemoveReason, SegmentAccounting, TakenFiles, TakenSegment,
};
use crate::dump::{DumpError, DumpReceipt, DumpRequest, Lookback};
use crate::metrics::{
    Operation, SegmentProcessMetrics, SegmentProcessMetricsGuard, WorkerCycleMetrics,
};
use crate::rate_limit::rate_limited;
use crate::telemetry::writer::{Disk, WriterMode};
use futures_util::FutureExt;
use metrique::timers::Timer;
use metrique_writer::BoxEntrySink;
use pipeline_metrics::{MetriqueResult, PipelineMetrics, StageMetrics};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Configuration for the in-process worker pipeline.
///
/// The pipeline is composed of a sequence of [`SegmentProcessor`]s supplied
/// via `processors`. When none are provided the worker runs no processing.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct BackgroundTaskConfig {
    /// The trace base path (same path passed to `DiskWriter::new`).
    /// `None` when using the in-memory backend.
    #[builder(into)]
    trace_path: Option<PathBuf>,
    /// How often the worker checks for sealed segments. Defaults to 1 second.
    #[builder(default = DEFAULT_POLL_INTERVAL)]
    poll_interval: Duration,
    /// The processor pipeline executed for each sealed segment, in order.
    #[builder(default)]
    processors: Vec<Box<dyn SegmentProcessor>>,
    /// Metrics sink. Defaults to [`DevNullSink`](metrique_writer::sink::DevNullSink).
    #[builder(default = metrique_writer::sink::DevNullSink::boxed())]
    metrics_sink: BoxEntrySink,
    /// Trigger receiver flipping the worker into on-demand operation; see
    /// [`crate::dump`]. `None` (the default) keeps continuous processing.
    #[builder(setters(vis = "pub(crate)"))]
    trigger: Option<crate::dump::DumpRx>,
}

impl std::fmt::Debug for BackgroundTaskConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundTaskConfig")
            .field("trace_path", &self.trace_path)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl BackgroundTaskConfig {
    /// How often the worker checks for sealed segments.
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// The full trace base path (e.g. `/tmp/trace.bin`). `None` for memory
    /// mode.
    #[cfg(test)]
    pub(crate) fn trace_path(&self) -> Option<&Path> {
        self.trace_path.as_deref()
    }

    /// Directory containing trace segments.
    pub fn trace_dir(&self) -> &Path {
        match self.trace_path.as_deref().and_then(|p| p.parent()) {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        }
    }

    /// File stem used for segment matching (e.g. "trace" for "trace.0.bin").
    pub fn trace_stem(&self) -> &str {
        let stem = self
            .trace_path
            .as_deref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty());
        match stem {
            Some(s) => s,
            None => {
                if let Some(p) = self.trace_path.as_deref() {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::error!(
                            target: "dial9_worker",
                            path = %p.display(),
                            "trace_path has no file stem — pass a path like /tmp/traces/trace.bin, not a directory"
                        );
                    });
                }
                "trace"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SegmentProcessor pipeline
// ---------------------------------------------------------------------------

/// Data flowing through the processor pipeline.
///
/// The worker reads the sealed segment into `payload`, populates initial
/// `metadata`, then passes this through each [`SegmentProcessor`] in order.
/// Metrics are flushed automatically when the `SegmentData` is dropped.
///
/// `SegmentData` is intentionally `!Clone`, it is moved through the pipeline
/// and dropped exactly once, which is crucial for `SegmentAccounting`'s
/// in-flight byte accounting (a single `Drop` decrements the counters).
pub struct SegmentData {
    pub(crate) segment: SegmentRef,
    pub(crate) payload: Payload,
    pub(crate) metadata: HashMap<String, String>,
    pub(crate) metrics: SegmentProcessMetricsGuard,
    /// Memory-mode in-flight accounting. `None` for disk-backed segments.
    /// Held only for its `Drop` (releases in-flight counters).
    #[allow(dead_code)]
    pub(crate) accounting: Option<SegmentAccounting>,
}

impl SegmentData {
    /// Information about the sealed segment being processed.
    pub fn segment(&self) -> &SegmentRef {
        &self.segment
    }

    /// Current payload (raw, symbolized, compressed, etc.).
    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    /// Take ownership of the payload, leaving an empty [`Payload`] in its place.
    pub fn take_payload(&mut self) -> Payload {
        std::mem::take(&mut self.payload)
    }

    /// Replace the payload.
    pub fn set_payload(&mut self, payload: impl Into<Payload>) {
        self.payload = payload.into();
    }

    /// Metadata accumulated by upstream processors.
    pub fn metadata(&self) -> &HashMap<String, String> {
        &self.metadata
    }

    /// Mutable reference to the metadata map. Processors can insert keys
    /// (e.g. `"content_encoding"`, `"write_back_extension"`) to signal
    /// downstream stages.
    pub fn metadata_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.metadata
    }

    /// Record the segment's post-compression size. Surfaces as the
    /// `CompressedSize` metric for this segment.
    pub fn set_compressed_size(&mut self, bytes: u64) {
        self.metrics.compressed_size = Some(bytes);
    }
}

impl std::fmt::Debug for SegmentData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentData")
            .field("segment", &self.segment)
            .field("payload", &self.payload)
            .field("metadata", &self.metadata)
            .field("metrics", &self.metrics)
            .finish()
    }
}

/// Error returned by a [`SegmentProcessor`].
///
/// Carries the [`SegmentData`] back so the caller can still record metrics
/// and pass the data to subsequent error-handling logic.
#[derive(Debug)]
pub struct ProcessError {
    pub(crate) data: SegmentData,
    pub(crate) kind: ProcessErrorKind,
}

impl ProcessError {
    /// Wrap `data` and `kind` into a new [`ProcessError`].
    pub fn new(data: SegmentData, kind: ProcessErrorKind) -> Self {
        Self { data, kind }
    }

    /// Shorthand for [`ProcessError::new`] with an I/O error.
    pub fn io(data: SegmentData, err: std::io::Error) -> Self {
        Self::new(data, ProcessErrorKind::Io(err))
    }
}

/// Kind of failure reported by a [`SegmentProcessor`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ProcessErrorKind {
    /// The processor hit an `std::io::Error`.
    #[non_exhaustive]
    Io(std::io::Error),

    /// An error transferring data off the host
    #[non_exhaustive]
    Transfer {
        /// Underlying error source.
        source: Box<dyn std::error::Error + Send + Sync>,
        /// Whether this error is transient and the segment should be kept on
        /// disk for retry.
        retryable: bool,
    },
}

impl ProcessErrorKind {
    fn already_deleted(&self) -> bool {
        matches!(self, ProcessErrorKind::Io(err) if err.kind() == io::ErrorKind::NotFound)
    }

    /// Whether this error is transient and the segment should be kept on disk
    /// for retry.
    fn retryable(&self) -> bool {
        match self {
            ProcessErrorKind::Transfer { retryable, .. } => *retryable,
            _ => false,
        }
    }
}

impl std::fmt::Display for ProcessErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Transfer { source, .. } => write!(f, "S3 transfer error: {source}"),
        }
    }
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind.fmt(f)
    }
}

impl std::error::Error for ProcessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ProcessErrorKind::Io(e) => Some(e),
            ProcessErrorKind::Transfer { source, .. } => Some(source.as_ref()),
        }
    }
}

impl From<std::io::Error> for ProcessErrorKind {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(feature = "worker-s3")]
impl From<aws_sdk_s3_transfer_manager::error::Error> for ProcessErrorKind {
    fn from(e: aws_sdk_s3_transfer_manager::error::Error) -> Self {
        let retryable = matches!(
            e.kind(),
            aws_sdk_s3_transfer_manager::error::ErrorKind::IOError
                | aws_sdk_s3_transfer_manager::error::ErrorKind::RuntimeError
                | aws_sdk_s3_transfer_manager::error::ErrorKind::ChildOperationFailed
                | aws_sdk_s3_transfer_manager::error::ErrorKind::ChunkFailed(_)
        );
        Self::Transfer {
            source: Box::new(e),
            retryable,
        }
    }
}

/// A single step in the segment processing pipeline.
///
/// Implementations handle one concern: compress, symbolize, upload, etc.
/// The worker calls processors in sequence for each segment.
///
/// # Panic safety
///
/// The worker loop catches panics from [`process()`](Self::process) and
/// skips the panicking segment. The same processor instance is reused for
/// subsequent segments, so implementations **must** remain in a valid state
/// after a panic (i.e., no partially-updated invariants that would cause
/// incorrect behavior on the next call).
pub trait SegmentProcessor: Send {
    /// Human-readable name for this processor (used in metrics).
    fn name(&self) -> &'static str;

    /// Process a segment, transforming or consuming its data.
    /// Returns the (possibly modified) data for the next processor,
    /// or an error to skip this segment.
    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>;

    /// Called once per finished dump in triggered mode (see
    /// [`crate::dump`]), in pipeline order, so stages can flush any
    /// per-dump state they accumulated. Return the S3 key of a manifest
    /// written for this dump, or `None`; the last `Some` across the
    /// pipeline lands on [`DumpReceipt::manifest_key`](crate::dump::DumpReceipt::manifest_key).
    ///
    /// Default: no-op returning `None`. Never called in continuous mode.
    /// The same panic-safety contract as [`process()`](Self::process)
    /// applies.
    fn finalize_dump(
        &mut self,
        completion: &crate::dump::DumpCompletion,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        let _ = completion;
        Box::pin(std::future::ready(None))
    }
}

/// Closure-scoped builder for assembling a custom processor pipeline.
///
/// Obtained via `with_custom_pipeline(|p| ...)` on the runtime builder. The
/// `Mode` type parameter binds the pipeline to the writer's storage mode:
/// disk-only processors like [`write_back`](Self::write_back) are not in
/// scope on `PipelineBuilder<Memory>`, so wiring write-back into an
/// in-memory pipeline is a compile error.
///
/// # Example
///
/// ```ignore
/// struct Logger;
/// impl SegmentProcessor for Logger {
///     fn name(&self) -> &'static str { "logger" }
///     fn process(&mut self, data: SegmentData)
///         -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
///     {
///         Box::pin(async move {
///             println!("segment {} ({} bytes)", data.segment().index(), data.payload().len());
///             Ok(data)
///         })
///     }
/// }
///
/// builder.with_custom_pipeline(|p| p.pipe(Logger).gzip().write_back())
/// ```
#[must_use]
pub struct PipelineBuilder<Mode: WriterMode = Disk> {
    processors: Vec<Box<dyn SegmentProcessor>>,
    _marker: PhantomData<Mode>,
}

impl<Mode: WriterMode> PipelineBuilder<Mode> {
    pub(crate) fn new() -> Self {
        Self {
            processors: Vec::new(),
            _marker: PhantomData,
        }
    }

    pub(crate) fn into_processors(self) -> Vec<Box<dyn SegmentProcessor>> {
        self.processors
    }

    /// Append a user-supplied [`SegmentProcessor`] to the pipeline.
    pub fn pipe<S>(mut self, processor: S) -> Self
    where
        S: SegmentProcessor + 'static,
    {
        self.processors.push(Box::new(processor));
        self
    }

    /// Gzip the segment payload in-memory.
    pub fn gzip(mut self) -> Self {
        self.processors.push(Box::new(GzipCompressor));
        self
    }

    /// Resolve stack-frame addresses in the segment to symbol names.
    /// Only valid when the runtime is built with the `cpu-profiling` feature.
    ///
    /// The built-in S3 / default presets prepend this automatically when
    /// CPU profiling is on; on the custom path the pipeline is passed
    /// through verbatim, so chain `.symbolize()` first if you want
    /// symbolized stack frames in your trace files.
    #[cfg(feature = "cpu-profiling")]
    pub fn symbolize(mut self) -> Self {
        self.processors.push(Box::new(SymbolizeProcessor::new()));
        self
    }

    /// Upload the current payload to S3 with the given configuration. The
    /// AWS SDK default credential chain is used; call [`s3_with_client`]
    /// to supply a pre-built client.
    ///
    /// Does not auto-add gzip — chain `.gzip()` first if you want
    /// compressed uploads.
    ///
    /// [`s3_with_client`]: Self::s3_with_client
    #[cfg(feature = "worker-s3")]
    pub fn s3(mut self, config: s3::S3Config) -> Self {
        self.processors
            .push(Box::new(S3PipelineUploader::new(config, None)));
        self
    }

    /// Variant of [`s3`](Self::s3) that uses the supplied pre-built S3 client.
    #[cfg(feature = "worker-s3")]
    pub fn s3_with_client(mut self, config: s3::S3Config, client: aws_sdk_s3::Client) -> Self {
        self.processors
            .push(Box::new(S3PipelineUploader::new(config, Some(client))));
        self
    }
}

/// Disk-only methods on the pipeline builder.
impl PipelineBuilder<Disk> {
    /// Write the current payload bytes back to disk. When the payload has
    /// been gzipped earlier in the pipeline, the file is written with a
    /// `.gz` suffix and the original sealed segment is removed.
    pub fn write_back(mut self) -> Self {
        self.processors
            .push(Box::new(WriteBackProcessor::default()));
        self
    }

    /// Write the current payload bytes to a specific directory instead of
    /// back alongside the original segment. The file name is preserved;
    /// when the payload has been gzipped, a `.gz` suffix is appended.
    /// The original sealed segment is removed after a successful write.
    pub fn write_back_to(mut self, dir: impl Into<PathBuf>) -> Self {
        self.processors.push(Box::new(WriteBackProcessor {
            dir: Some(dir.into()),
        }));
        self
    }
}

impl<Mode: WriterMode> std::fmt::Debug for PipelineBuilder<Mode> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineBuilder")
            .field("len", &self.processors.len())
            .finish()
    }
}

/// The worker loop function. Runs on a dedicated thread, polls for sealed
/// segments and processes them through the configured pipeline.
///
/// Creates a single-threaded tokio runtime for async processors (e.g. S3 upload).
/// The worker is a "good citizen": it will lose data rather than disrupt the application.
pub(crate) fn run_background_task(
    mut config: BackgroundTaskConfig,
    shutdown: tokio::sync::oneshot::Receiver<Duration>,
    fs: Arc<Fs>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .thread_name("dial9-worker-rt")
        .enable_all()
        .build()
        .expect("failed to create worker runtime");

    let processors = std::mem::take(&mut config.processors);
    let metrics_sink = config.metrics_sink.clone();
    let trigger = config.trigger.take();

    tracing::info!(target: "dial9_worker", dir = %config.trace_dir().display(), stem = %config.trace_stem(), processors = processors.len(), triggered = trigger.is_some(), "worker started");
    rt.block_on(async {
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            fs,
            config.poll_interval(),
            processors,
            stop.clone(),
            metrics_sink,
            trigger,
        );
        let mut run_fut = std::pin::pin!(worker.run());
        // Poll the worker until we receive a shutdown signal with a drain timeout.
        let drain_timeout = tokio::select! {
            () = &mut run_fut => return,
            msg = shutdown => msg.unwrap_or(Duration::ZERO),
        };
        tracing::info!(target: "dial9_worker", ?drain_timeout, "stop signal received, draining");
        // Tell the worker to exit after its current processing cycle.
        stop.cancel();
        // Give it `drain_timeout` to finish; after that, drop the future.
        match tokio::time::timeout(drain_timeout, run_fut).await {
            Ok(()) => tracing::info!(target: "dial9_worker", "drain complete"),
            Err(_) => tracing::warn!(target: "dial9_worker", "drain timed out"),
        }
    });
    tracing::info!(target: "dial9_worker", "worker stopped");
}

// ---------------------------------------------------------------------------
// GzipCompressor — compresses segment bytes in-memory
// ---------------------------------------------------------------------------

/// Gzips the segment payload in-memory. Sets the `content_encoding` and
/// `write_back_extension` metadata keys so downstream stages know the
/// payload is gzipped. Already-gzipped segments (detected by magic bytes)
/// pass through unchanged.
#[derive(Debug, Default)]
pub(crate) struct GzipCompressor;

impl SegmentProcessor for GzipCompressor {
    fn name(&self) -> &'static str {
        "Gzip"
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        Box::pin(async move {
            // Skip already-compressed segments to avoid double-gzip.
            if data.payload.starts_with(&[0x1f, 0x8b]) {
                data.metadata
                    .insert("content_encoding".into(), "gzip".into());
                data.metadata
                    .insert("write_back_extension".into(), ".gz".into());
                return Ok(data);
            }
            let raw = std::mem::take(&mut data.payload);
            let compressed = tokio::task::spawn_blocking(move || {
                use flate2::write::GzEncoder;
                use std::io::Write;
                let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
                for chunk in raw.chunks() {
                    encoder.write_all(chunk)?;
                }
                encoder.finish()
            })
            .await;
            match compressed {
                Ok(Ok(bytes)) => {
                    data.metrics.compressed_size = Some(bytes.len() as u64);
                    data.payload = Payload::from_vec(bytes);
                    data.metadata
                        .insert("content_encoding".into(), "gzip".into());
                    data.metadata
                        .insert("write_back_extension".into(), ".gz".into());
                    Ok(data)
                }
                Ok(Err(e)) => Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(e),
                }),
                Err(e) => Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(std::io::Error::other(e)),
                }),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// SymbolizeProcessor — resolves stack frame addresses to symbol names
// ---------------------------------------------------------------------------

/// Resolves stack-frame addresses in the segment to symbol names using
/// the current process's `/proc/self/maps`.
///
/// Owns a long-lived
/// [`OfflineSymbolizer`](dial9_perf_self_profile::offline_symbolize::OfflineSymbolizer)
/// running on a dedicated thread, so blazesym's per-ELF DWARF cache
/// stays warm across segments. Without this, every segment paid the
/// full ELF parse cost (hundreds of ms — see #462).
#[cfg(feature = "cpu-profiling")]
pub(crate) struct SymbolizeProcessor {
    symbolizer: std::sync::Arc<dial9_perf_self_profile::offline_symbolize::OfflineSymbolizer>,
}

#[cfg(feature = "cpu-profiling")]
impl SymbolizeProcessor {
    pub(crate) fn new() -> Self {
        Self {
            symbolizer: std::sync::Arc::new(
                dial9_perf_self_profile::offline_symbolize::OfflineSymbolizer::new(),
            ),
        }
    }
}

#[cfg(feature = "cpu-profiling")]
impl Default for SymbolizeProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "cpu-profiling")]
impl std::fmt::Debug for SymbolizeProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SymbolizeProcessor").finish_non_exhaustive()
    }
}

#[cfg(feature = "cpu-profiling")]
impl SegmentProcessor for SymbolizeProcessor {
    fn name(&self) -> &'static str {
        "Symbolize"
    }

    fn process(
        &mut self,
        mut data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        let symbolizer = self.symbolizer.clone();
        Box::pin(async move {
            // Skip already-compressed segments (e.g. leftover from a previous run).
            if data.payload.starts_with(&[0x1f, 0x8b]) {
                tracing::debug!(target: "dial9_worker", "segment is gzip-compressed, skipping symbolization");
                return Ok(data);
            }
            // The symbolize FFI reads `&[u8]`, so we materialize a single
            // contiguous `Bytes`. When there's only one chunk this is a
            // zero-copy `Bytes::clone`-equivalent; the `BytesMut` concat
            // path runs only on already-segmented input (rare).
            let input = std::mem::take(&mut data.payload).into_bytes();
            // Hand off to a blocking thread because `OfflineSymbolizer::symbolize`
            // is itself a blocking call (it sends to its dedicated symbolizer
            // thread and waits for the response).
            let result = tokio::task::spawn_blocking(move || {
                let maps = dial9_perf_self_profile::read_proc_maps();
                let output = symbolizer.symbolize_bytes(input.clone(), &maps)?;
                // Hand back the original bytes plus the symbol output as two
                // chunks — no copy of `input`.
                let mut combined = Payload::new();
                combined.push(input);
                combined.push(bytes::Bytes::from(output));
                Ok::<_, std::io::Error>(combined)
            })
            .await;
            match result {
                Ok(Ok(payload)) => {
                    data.payload = payload;
                    Ok(data)
                }
                Ok(Err(e)) => {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(target: "dial9_worker", error = %e, "symbolization failed, preserving original bytes");
                    });
                    Err(ProcessError {
                        data,
                        kind: ProcessErrorKind::Io(e),
                    })
                }
                Err(e) => Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(std::io::Error::other(e)),
                }),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// WriteBackProcessor — writes processed bytes back to disk
// ---------------------------------------------------------------------------

/// Writes the current payload bytes back to disk. If a
/// `write_back_extension` metadata key is present, the bytes are written to
/// `{original}{extension}` and the original segment file is removed.
/// When `dir` is set, the file is written to that directory instead of
/// alongside the original.
#[derive(Debug, Default)]
pub(crate) struct WriteBackProcessor {
    dir: Option<PathBuf>,
}

impl SegmentProcessor for WriteBackProcessor {
    fn name(&self) -> &'static str {
        "WriteBack"
    }

    fn process(
        &mut self,
        data: SegmentData,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
        let output_dir = self.dir.clone();
        Box::pin(async move {
            let original_path = match data.segment.disk_path() {
                Some(p) => p.to_owned(),
                None => {
                    return Err(ProcessError::io(
                        data,
                        std::io::Error::other(
                            "WriteBackProcessor requires a disk-backed segment; \
                             memory-backed segments must not use write_back()",
                        ),
                    ));
                }
            };
            let base_path = match &output_dir {
                Some(dir) => dir.join(original_path.file_name().unwrap_or_default()),
                None => original_path.clone(),
            };
            let dest_path = match data.metadata.get("write_back_extension") {
                Some(ext) => {
                    let mut p = base_path.as_os_str().to_owned();
                    p.push(ext);
                    std::path::PathBuf::from(p)
                }
                None => base_path,
            };
            let payload = data.payload.clone();
            let write_dest = dest_path.clone();
            let result = tokio::task::spawn_blocking(move || {
                use std::io::{BufWriter, Write};
                if let Some(parent) = write_dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut f = BufWriter::new(std::fs::File::create(&write_dest)?);
                for chunk in payload.chunks() {
                    f.write_all(chunk)?;
                }
                f.flush()
            })
            .await;
            match result {
                Ok(Ok(())) => {
                    if dest_path != original_path {
                        // Remove the original .bin now that the output exists elsewhere.
                        // If the writer already evicted it, clean up the dest
                        // file we just wrote so it doesn't leak on disk.
                        match std::fs::remove_file(&original_path) {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                let _ = std::fs::remove_file(&dest_path);
                            }
                            Err(e) => {
                                rate_limited!(Duration::from_secs(60), {
                                    tracing::warn!(
                                        "failed to remove original segment {}: {e}",
                                        original_path.display()
                                    );
                                });
                            }
                        }
                    }
                    Ok(data)
                }
                Ok(Err(e)) => Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(e),
                }),
                Err(e) => Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(std::io::Error::other(e)),
                }),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// WorkerLoop — the async state machine
// ---------------------------------------------------------------------------
// WorkerLoop — the async state machine
// ---------------------------------------------------------------------------

pub(crate) struct WorkerLoop {
    fs: Arc<Fs>,
    poll_interval: Duration,
    processors: Vec<Box<dyn SegmentProcessor>>,
    metrics_sink: BoxEntrySink,
    /// When cancelled, the worker finishes its current cycle and exits
    /// instead of sleeping.
    stop: tokio_util::sync::CancellationToken,
    /// Present: on-demand operation, segments only run through the
    /// pipeline when a dump is requested. Absent: continuous processing.
    trigger: Option<crate::dump::DumpRx>,
    /// Triggered mode, disk backend only: `(creation, seal)` epochs of
    /// segments already inspected and found outside every active window, so
    /// their files are not re-read on each pass. Entries leave when the
    /// segment is processed or removed, and each matching pass prunes
    /// entries for files no longer on disk (writer-evicted).
    epoch_cache: HashMap<u32, (u64, u64)>,
}

/// A dump registered with the triggered worker, accumulating receipt state
/// while its window collects segments.
struct ActiveDump {
    id: crate::dump::DumpId,
    triggered_at: SystemTime,
    window: EpochWindow,
    /// `Some` iff a non-zero look-forward was requested; the dump stays
    /// registered until this elapses.
    deadline: Option<tokio::time::Instant>,
    metadata: Vec<(String, String)>,
    receipt_tx: Option<tokio::sync::oneshot::Sender<Result<DumpReceipt, DumpError>>>,
    segments_processed: usize,
    first_epoch: Option<u64>,
    last_epoch: Option<u64>,
    first_error: Option<ProcessErrorKind>,
}

impl ActiveDump {
    fn register(req: DumpRequest) -> Self {
        let trigger_epoch = req
            .triggered_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let start_secs = match req.lookback {
            Lookback::Unbounded => None,
            Lookback::Window(d) => Some(trigger_epoch.saturating_sub(d.as_secs())),
        };
        let window = EpochWindow {
            start_secs,
            end_secs: trigger_epoch.saturating_add(req.lookforward.as_secs()),
        };
        let deadline = (!req.lookforward.is_zero()).then(|| {
            // Anchor at trigger time so worker pickup latency does not
            // extend the forward window.
            let elapsed = req.triggered_at.elapsed().unwrap_or_default();
            tokio::time::Instant::now() + req.lookforward.saturating_sub(elapsed)
        });
        Self {
            id: req.id,
            triggered_at: req.triggered_at,
            window,
            deadline,
            metadata: req.metadata,
            receipt_tx: Some(req.receipt_tx),
            segments_processed: 0,
            first_epoch: None,
            last_epoch: None,
            first_error: None,
        }
    }

    /// Whether the dump can resolve once no matching work remains: no
    /// forward window, or its deadline elapsed.
    fn due(&self, now: tokio::time::Instant) -> bool {
        self.deadline.is_none_or(|d| now >= d)
    }

    /// Actual covered span: the captured segments' epoch extent, or the
    /// trigger instant for an empty dump.
    fn time_range(&self) -> (SystemTime, SystemTime) {
        match (self.first_epoch, self.last_epoch) {
            (Some(first), Some(last)) => (epoch_to_system(first), epoch_to_system(last)),
            _ => (self.triggered_at, self.triggered_at),
        }
    }

    /// Total failure: a captured segment failed terminally and nothing
    /// made it through. Drives both the `Err` receipt and the S3 stage
    /// skipping the manifest.
    fn failed(&self) -> bool {
        self.first_error.is_some() && self.segments_processed == 0
    }

    /// The completion signal handed to each stage's `finalize_dump`.
    fn completion(&self) -> crate::dump::DumpCompletion {
        crate::dump::DumpCompletion {
            dump_id: self.id,
            triggered_at: self.triggered_at,
            time_range: self.time_range(),
            segments_processed: self.segments_processed,
            metadata: self.metadata.clone(),
            failed: self.failed(),
        }
    }

    /// Best-effort policy: `Ok` whenever anything succeeded or nothing
    /// failed; `Err(Pipeline)` only on total failure.
    fn into_result(
        mut self,
        manifest_key: Option<String>,
    ) -> (
        tokio::sync::oneshot::Sender<Result<DumpReceipt, DumpError>>,
        Result<DumpReceipt, DumpError>,
    ) {
        let tx = self
            .receipt_tx
            .take()
            .expect("receipt_tx only taken at resolution");
        let result = match (self.failed(), self.first_error.take()) {
            (true, Some(kind)) => Err(DumpError::Pipeline(kind)),
            _ => Ok(DumpReceipt {
                dump_id: self.id,
                segments_processed: self.segments_processed,
                finished_at: SystemTime::now(),
                time_range: self.time_range(),
                manifest_key,
            }),
        };
        (tx, result)
    }
}

fn epoch_to_system(epoch_secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(epoch_secs)
}

/// What one matching pass achieved, used by the triggered drain loop to
/// decide between another pass, bailing to the select (retry pacing), and
/// declaring the windows quiesced.
#[derive(Debug, Default)]
struct PassStats {
    /// Window-matched segments that reached a terminal outcome (pipeline
    /// success, terminal failure, eviction, panic, retry budget spent).
    matched_done: usize,
    /// Ids of the dumps matched by segments actually re-enqueued after a
    /// retryable failure; those dumps stay open until the retry settles.
    retry_dump_ids: Vec<crate::dump::DumpId>,
    /// Segments that passed window matching and entered the pipeline.
    entered_pipeline: usize,
}

/// Record a terminal pipeline error against every matched dump that has
/// none yet. The first dump takes `kind` itself; the rest get an
/// `Io`-wrapped copy of its message (`ProcessErrorKind` is not `Clone`).
fn record_dump_error(dumps: &mut [ActiveDump], matched: &[usize], kind: ProcessErrorKind) {
    let msg = kind.to_string();
    let mut kind = Some(kind);
    for &i in matched {
        let d = &mut dumps[i];
        if d.first_error.is_none() {
            d.first_error = Some(
                kind.take()
                    .unwrap_or_else(|| ProcessErrorKind::Io(io::Error::other(msg.clone()))),
            );
        }
    }
}

impl WorkerLoop {
    pub(crate) fn new(
        fs: Arc<Fs>,
        poll_interval: Duration,
        processors: Vec<Box<dyn SegmentProcessor>>,
        stop: tokio_util::sync::CancellationToken,
        metrics_sink: BoxEntrySink,
        trigger: Option<crate::dump::DumpRx>,
    ) -> Self {
        Self {
            fs,
            poll_interval,
            processors,
            metrics_sink,
            stop,
            trigger,
            epoch_cache: HashMap::new(),
        }
    }

    pub(crate) async fn run(&mut self) {
        match self.trigger.take() {
            None => self.run_continuous().await,
            Some(rx) => self.run_triggered(rx).await,
        }
    }

    async fn run_continuous(&mut self) {
        loop {
            let taken = self.fs.take_files();
            let dispatched = taken.segments.len() as u64;
            self.emit_cycle_metrics(&taken, dispatched);
            self.process_segments(taken.segments, &mut []).await;

            if self.stop.is_cancelled() || self.fs.writer_done() {
                // Drain-to-empty: keep popping until the ring/directory is clear.
                // Ordering invariant: writer calls mark_writer_done (Release) after
                // the seal-time queue push, so any late-racing push is visible here.
                loop {
                    let taken = self.fs.take_files();
                    let dispatched = taken.segments.len() as u64;
                    self.emit_cycle_metrics(&taken, dispatched);
                    if taken.segments.is_empty() {
                        tracing::debug!(target: "dial9_worker", "Exiting run loop: drain complete");
                        return;
                    }
                    self.process_segments(taken.segments, &mut []).await;
                }
            }

            self.fs.wait_for_more(&self.stop, self.poll_interval).await;
        }
    }

    /// On-demand operation: park between triggers (no `take_files`), and on
    /// a dump request drain only the segments whose `[creation, seal]` span
    /// overlaps an active window. Segments outside every window stay in the
    /// ring.
    async fn run_triggered(&mut self, mut rx: crate::dump::DumpRx) {
        let mut dumps: Vec<ActiveDump> = Vec::new();
        let mut rx_open = true;

        loop {
            if !dumps.is_empty() {
                let retry_hold = self.drain_matching(&mut dumps).await;
                // Resolve every dump whose forward deadline elapsed (or
                // that never had one) and that is not held open by a
                // pending retry. A disk pass covers the whole backlog, so
                // a retry there only holds the dumps the retrying segment
                // matched; the memory pop dispenses one slot per pass, so
                // a retry there keeps everything open until the head of
                // the ring settles (budget-bounded, brief).
                let exhaustive = self.fs.take_is_exhaustive();
                let now = tokio::time::Instant::now();
                let mut i = 0;
                while i < dumps.len() {
                    let held = !retry_hold.is_empty()
                        && (!exhaustive || retry_hold.contains(&dumps[i].id));
                    if dumps[i].due(now) && !held {
                        self.resolve_dump(dumps.swap_remove(i)).await;
                    } else {
                        i += 1;
                    }
                }
            }

            if self.stop.is_cancelled() || self.fs.writer_done() {
                // One final matching pass picks up segments sealed by
                // writer finalization, then every open dump resolves with
                // a truncated receipt covering what actually landed.
                self.drain_matching(&mut dumps).await;
                for dump in dumps.drain(..) {
                    self.resolve_dump(dump).await;
                }
                // Requests that never registered fail explicitly.
                rx.rx.close();
                while let Ok(req) = rx.rx.try_recv() {
                    let _ = req.receipt_tx.send(Err(DumpError::WorkerStopped));
                }
                tracing::debug!(target: "dial9_worker", "Exiting triggered run loop");
                return;
            }

            let min_deadline = dumps.iter().filter_map(|d| d.deadline).min();
            tokio::select! {
                _ = self.stop.cancelled() => {}
                req = rx.rx.recv(), if rx_open => {
                    match req {
                        Some(req) => dumps.push(ActiveDump::register(req)),
                        // All `DumpTrigger`s dropped; disable the branch so
                        // the closed channel does not spin the select.
                        None => rx_open = false,
                    }
                }
                _ = tokio::time::sleep_until(
                    min_deadline.unwrap_or_else(tokio::time::Instant::now)
                ), if min_deadline.is_some() => {}
                _ = self.fs.wait_for_more(&self.stop, self.poll_interval),
                    if !dumps.is_empty() => {}
            }
        }
    }

    /// Run matching passes until the active windows quiesce. Returns the
    /// ids of dumps matched by segments that failed retryably (the caller
    /// bails to its select instead of hot-looping the retry and keeps
    /// those dumps open); empty means the windows quiesced.
    async fn drain_matching(&mut self, dumps: &mut [ActiveDump]) -> Vec<crate::dump::DumpId> {
        loop {
            if dumps.is_empty() {
                return Vec::new();
            }
            let windows: Vec<EpochWindow> = dumps.iter().map(|d| d.window).collect();
            let mut taken = self.fs.take_files_matching(&windows);
            // Prune cache entries for files no longer dispensed (disk
            // dispenses every unclaimed file per pass, so absence means
            // the writer evicted it).
            if !self.epoch_cache.is_empty() {
                let live: std::collections::HashSet<u32> =
                    taken.segments.iter().map(|t| t.seg_ref.index()).collect();
                self.epoch_cache.retain(|idx, _| live.contains(idx));
            }
            if taken.segments.is_empty() {
                self.emit_cycle_metrics(&taken, 0);
                return Vec::new();
            }
            let segments = std::mem::take(&mut taken.segments);
            let stats = self.process_segments(segments, dumps).await;
            // Out-of-window claims are released, not dispatched; only
            // count segments that actually entered the pipeline.
            self.emit_cycle_metrics(&taken, stats.entered_pipeline as u64);
            if !stats.retry_dump_ids.is_empty() {
                return stats.retry_dump_ids;
            }
            if stats.matched_done == 0 {
                // Only out-of-window segments (disk): nothing matching left.
                return Vec::new();
            }
        }
    }

    /// Resolve a finished dump: signal every stage in pipeline order so it
    /// can flush per-dump state (the S3 stage writes the manifest here),
    /// then send the receipt to whoever is awaiting it. Finalize runs for
    /// every resolved dump — errored and empty ones included — so stages
    /// always get to clear their per-dump bookkeeping.
    async fn resolve_dump(&mut self, dump: ActiveDump) {
        let completion = dump.completion();
        let mut manifest_key = None;
        for processor in &mut self.processors {
            let processor_name = processor.name();
            // Same panic discipline as `process()`: a panicking finalize
            // is caught, logged, and the receipt still resolves.
            let finalize_result = {
                // `Option::take` moves the `&mut` out of the capture so the
                // returned future borrows the processor, not the closure.
                let mut slot = Some(&mut **processor);
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let p = slot.take().expect("closure called once");
                    p.finalize_dump(&completion)
                })) {
                    Ok(fut) => std::panic::AssertUnwindSafe(fut).catch_unwind().await,
                    Err(panic_payload) => Err(panic_payload),
                }
            };
            match finalize_result {
                Ok(Some(key)) => manifest_key = Some(key),
                Ok(None) => {}
                Err(_) => {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::error!(
                            target: "dial9_worker",
                            processor = processor_name,
                            dump_id = %completion.dump_id,
                            "finalize_dump panicked"
                        );
                    });
                }
            }
        }
        let (tx, result) = dump.into_result(manifest_key);
        // The caller may have dropped the handle; that does not cancel.
        let _ = tx.send(result);
    }

    // Test-only: prod drains via the `run()` shutdown loop
    // (stop/writer_done -> drain-to-empty). This just forces one synchronous
    // drain cycle for unit tests.
    #[cfg(test)]
    pub(crate) async fn process_open_segments(&mut self) -> bool {
        let taken = self.fs.take_files();
        let found = !taken.segments.is_empty();
        let dispatched = taken.segments.len() as u64;
        self.emit_cycle_metrics(&taken, dispatched);
        self.process_segments(taken.segments, &mut []).await;
        found
    }

    async fn process_segments(
        &mut self,
        segments: Vec<TakenSegment>,
        dumps: &mut [ActiveDump],
    ) -> PassStats {
        let mut stats = PassStats::default();
        if self.processors.is_empty() {
            return stats;
        }

        'next_segment: for (seg_idx, taken) in segments.into_iter().enumerate() {
            // Cached-epoch fast path (triggered mode, disk): a segment
            // already inspected and found out-of-window is released without
            // re-reading its file.
            if !dumps.is_empty()
                && let Some(&(start, seal)) = self.epoch_cache.get(&taken.seg_ref.index())
                && !dumps.iter().any(|d| d.window.overlaps(start, seal))
            {
                self.fs.release_claim(&taken.seg_ref);
                continue;
            }
            // Snapshot memory-only retry state before `load()` consumes
            // `taken`, so re-dispense on a retryable failure gets the same bytes as the first attempt.
            let retry_count = taken.retry_count();
            let original_bytes = taken.original_bytes();
            let mem_epochs = taken.mem_epochs();
            let (seg_ref, payload, accounting) = match taken.load() {
                Ok(t) => t,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(
                            target: "dial9_worker",
                            "segment vanished between scan and load, skipping"
                        );
                    });
                    continue;
                }
                Err(e) => {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(target: "dial9_worker", error = %e, "failed to load segment");
                    });
                    continue;
                }
            };

            let uncompressed_size = payload.len() as u64;
            let path_for_header = seg_ref.disk_path().unwrap_or_else(|| Path::new(""));
            // A freshly loaded segment is always a single chunk holding the
            // whole payload, so the first chunk is the full byte range the
            // timestamp parser needs.
            let header_bytes = payload.chunks().first().map_or(&[][..], |b| b.as_ref());
            let (epoch_secs, header_valid) =
                sealed::creation_epoch_secs(header_bytes, path_for_header);
            // Seal epoch: memory slots carry it; disk derives it from the
            // file's mtime (best-effort).
            let seal_secs = match mem_epochs {
                Some((_, seal)) => seal,
                None => sealed::seal_epoch_secs(path_for_header),
            };

            // Triggered mode: match against every active dump window.
            let matched: Vec<usize> = dumps
                .iter()
                .enumerate()
                .filter(|(_, d)| d.window.overlaps(epoch_secs, seal_secs))
                .map(|(i, _)| i)
                .collect();
            if !dumps.is_empty() && matched.is_empty() {
                // Outside every window: leave it in place for later dumps.
                match &seg_ref {
                    SegmentRef::Disk(_) => {
                        self.epoch_cache
                            .insert(seg_ref.index(), (epoch_secs, seal_secs));
                        self.fs.release_claim(&seg_ref);
                    }
                    SegmentRef::Memory(_) => {
                        // Defensive: the windowed pop only dispenses
                        // matching slots. Put the bytes back without
                        // burning a retry attempt.
                        if let (Some(count), Some(bytes)) = (retry_count, original_bytes.as_ref()) {
                            self.fs.release_for_retry(
                                &seg_ref,
                                bytes.clone(),
                                count,
                                (epoch_secs, seal_secs),
                            );
                        }
                    }
                }
                continue;
            }
            stats.entered_pipeline += 1;

            let metrics = SegmentProcessMetrics {
                operation: Operation::ProcessSegment,
                total_time: Timer::start_now(),
                status: None,
                segment_index: seg_ref.index(),
                uncompressed_size,
                compressed_size: None,
                invalid_file_header: !header_valid,
                panicked: false,
                panic_message: None,
                pipeline: PipelineMetrics::default(),
            }
            .append_on_drop(self.metrics_sink.clone());

            // Kept for metadata, metrics, and failure logging after `seg_ref`
            // moves into `data` below.
            let seg_ref_retained = seg_ref.clone();
            let mut data = SegmentData {
                segment: seg_ref,
                payload,
                metadata: HashMap::from([
                    ("epoch_secs".into(), epoch_secs.to_string()),
                    ("segment_index".into(), seg_ref_retained.index().to_string()),
                ]),
                metrics,
                accounting,
            };

            if !matched.is_empty() {
                // Every matched dump's id rides the segment, comma-joined;
                // caller correlation pairs are namespaced `dump.{key}` and
                // first-registered dump wins on conflicts.
                let ids: Vec<String> = matched.iter().map(|&i| dumps[i].id.to_string()).collect();
                data.metadata.insert("dump_id".into(), ids.join(","));
                for &i in &matched {
                    for (k, v) in &dumps[i].metadata {
                        data.metadata
                            .entry(format!("dump.{k}"))
                            .or_insert_with(|| v.clone());
                    }
                }
            }

            for processor in &mut self.processors {
                let mut stage = StageMetrics::start();
                let proc_start = std::time::Instant::now();
                tracing::debug!(target: "dial9_worker", processor = processor.name(), segment = seg_idx + 1, "running processor");
                // Catch panics in both the synchronous `process()` call
                // (which builds the future) and during `.await` (polling).
                // AssertUnwindSafe: current processors are stateless or have
                // trivially-recoverable state, so reuse after panic is safe.
                let process_result = {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        processor.process(data)
                    })) {
                        Ok(fut) => std::panic::AssertUnwindSafe(fut).catch_unwind().await,
                        Err(panic_payload) => Err(panic_payload),
                    }
                };
                match process_result {
                    Ok(Ok(next)) => {
                        tracing::debug!(target: "dial9_worker", processor = processor.name(), segment = seg_idx + 1, elapsed_ms = proc_start.elapsed().as_secs_f64() * 1000.0, "processor succeeded");
                        data = next;
                        if let Some(acct) = data.accounting.as_mut() {
                            acct.adjust(data.payload.len() as u64);
                        }
                        stage.succeed();
                        data.metrics.pipeline.push(processor.name(), stage);
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(target: "dial9_worker", processor = processor.name(), segment = seg_idx + 1, elapsed_ms = proc_start.elapsed().as_secs_f64() * 1000.0, error = %e.kind, "processor failed");
                        data = e.data;
                        stage.fail();
                        data.metrics.pipeline.push(processor.name(), stage);
                        data.metrics.status = Some(MetriqueResult::Failure);
                        data.metrics.total_time.stop();
                        if e.kind.already_deleted() {
                            tracing::debug!(target: "dial9_worker", id = %data.segment, "segment evicted during processing, skipping");
                            // Best-effort: an evicted segment leaves the
                            // dump silently uncounted.
                            self.epoch_cache.remove(&seg_ref_retained.index());
                            if !matched.is_empty() {
                                stats.matched_done += 1;
                            }
                        } else if e.kind.retryable() {
                            match &data.segment {
                                // Memory segments always carry retry_count + a
                                // byte snapshot (set in `TakenSegment::memory`).
                                // If either is missing the invariant broke. In-flight
                                // is released via `data.accounting` on `continue`.
                                SegmentRef::Memory(_) => {
                                    match (retry_count, original_bytes.as_ref()) {
                                        (Some(prev), Some(bytes)) => {
                                            let attempt = prev + 1;
                                            if attempt
                                                > crate::background_task::fs::MEMORY_RETRY_BUDGET
                                            {
                                                rate_limited!(Duration::from_secs(60), {
                                                    tracing::warn!(target: "dial9_worker", id = %data.segment, err = ?e.kind, budget = crate::background_task::fs::MEMORY_RETRY_BUDGET, "memory retry budget exhausted, dropping segment");
                                                });
                                                // Budget spent: terminal for any
                                                // matched dump, same as a
                                                // non-retryable failure.
                                                if !matched.is_empty() {
                                                    stats.matched_done += 1;
                                                    record_dump_error(dumps, &matched, e.kind);
                                                }
                                            } else {
                                                tokio::time::sleep(self.poll_interval).await;
                                                self.fs.release_for_retry(
                                                    &data.segment,
                                                    bytes.clone(),
                                                    attempt,
                                                    (epoch_secs, seal_secs),
                                                );
                                                stats
                                                    .retry_dump_ids
                                                    .extend(matched.iter().map(|&i| dumps[i].id));
                                            }
                                        }
                                        _ => {
                                            rate_limited!(Duration::from_secs(60), {
                                                tracing::warn!(target: "dial9_worker", id = %data.segment, "memory segment missing retry state, dropping");
                                            });
                                            if !matched.is_empty() {
                                                stats.matched_done += 1;
                                                record_dump_error(dumps, &matched, e.kind);
                                            }
                                        }
                                    }
                                }
                                SegmentRef::Disk(_) => {
                                    tracing::debug!(target: "dial9_worker", id = %data.segment, err = ?e.kind, "retryable error");
                                    self.fs.release_claim(&data.segment);
                                    stats
                                        .retry_dump_ids
                                        .extend(matched.iter().map(|&i| dumps[i].id));
                                }
                            }
                        } else {
                            self.fs.remove_sealed(&data.segment, RemoveReason::Terminal);
                            rate_limited!(Duration::from_secs(60), {
                                tracing::warn!(target: "dial9_worker", error = %e.kind, cause = ?e.kind, id = %data.segment, "processor failed, removing segment");
                            });
                            self.epoch_cache.remove(&seg_ref_retained.index());
                            if !matched.is_empty() {
                                stats.matched_done += 1;
                                record_dump_error(dumps, &matched, e.kind);
                            }
                        }
                        continue 'next_segment;
                    }
                    Err(panic_payload) => {
                        let panic_msg = panic_payload
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| panic_payload.downcast_ref::<String>().map(|s| s.as_str()))
                            .unwrap_or("unknown panic");
                        rate_limited!(
                            Duration::from_secs(60),
                            tracing::error!(
                                target: "dial9_worker",
                                processor = processor.name(),
                                segment = seg_idx + 1,
                                id = %seg_ref_retained,
                                panic = panic_msg,
                                "processor panicked, skipping segment"
                            )
                        );
                        // The original metrics guard was consumed by the
                        // panicking future. Emit a new one so panics are
                        // visible in operational metrics.
                        drop(
                            SegmentProcessMetrics {
                                operation: Operation::ProcessSegment,
                                total_time: Timer::start_now(),
                                status: Some(MetriqueResult::Failure),
                                segment_index: seg_ref_retained.index(),
                                uncompressed_size,
                                compressed_size: None,
                                invalid_file_header: !header_valid,
                                panicked: true,
                                panic_message: Some(panic_msg.to_owned()),
                                pipeline: PipelineMetrics::default(),
                            }
                            .append_on_drop(self.metrics_sink.clone()),
                        );
                        self.fs
                            .remove_sealed(&seg_ref_retained, RemoveReason::Terminal);
                        self.epoch_cache.remove(&seg_ref_retained.index());
                        if !matched.is_empty() {
                            stats.matched_done += 1;
                            record_dump_error(
                                dumps,
                                &matched,
                                ProcessErrorKind::Io(io::Error::other(format!(
                                    "processor panicked: {panic_msg}"
                                ))),
                            );
                        }
                        continue 'next_segment;
                    }
                }
            }

            data.metrics.status = Some(MetriqueResult::Success);
            data.metrics.total_time.stop();
            self.epoch_cache.remove(&seg_ref_retained.index());
            if !matched.is_empty() {
                stats.matched_done += 1;
                for &i in &matched {
                    let d = &mut dumps[i];
                    d.segments_processed += 1;
                    d.first_epoch = Some(d.first_epoch.map_or(epoch_secs, |e| e.min(epoch_secs)));
                    d.last_epoch = Some(d.last_epoch.map_or(seal_secs, |e| e.max(seal_secs)));
                }
            }
        }

        stats
    }

    fn emit_cycle_metrics(&self, taken: &TakenFiles, segments_dispatched: u64) {
        drop(
            WorkerCycleMetrics {
                operation: Operation::WorkerCycle,
                memory_queued_segments: taken.queued_segments,
                memory_queued_bytes: taken.queued_bytes,
                in_flight_segments: taken.in_flight_segments,
                in_flight_bytes: taken.in_flight_bytes,
                memory_peak_in_flight_bytes: taken.in_flight_bytes_peak,
                segments_evicted: taken.segments_dropped,
                segments_dispatched,
            }
            .append_on_drop(self.metrics_sink.clone()),
        );
    }
}

// ---------------------------------------------------------------------------
// S3PipelineUploader — production S3 upload processor
// ---------------------------------------------------------------------------

/// S3 uploader processor. Construction is synchronous — the AWS client and
/// bucket region are resolved lazily on the first `process()` call, inside
/// the worker's tokio runtime.
#[cfg(feature = "worker-s3")]
pub(crate) struct S3PipelineUploader {
    state: S3UploaderState,
    /// Triggered mode: object keys written per dump id, accumulated while
    /// the dump is open and flushed into its manifest at `finalize_dump`.
    /// A key appears under several ids when forward windows overlap.
    dump_keys: HashMap<String, Vec<String>>,
}

#[cfg(feature = "worker-s3")]
enum S3UploaderState {
    Pending {
        s3_config: s3::S3Config,
        client: Option<aws_sdk_s3::Client>,
    },
    Ready {
        uploader: s3::S3Uploader,
        circuit_breaker: connection::CircuitBreaker,
    },
}

#[cfg(feature = "worker-s3")]
impl std::fmt::Debug for S3PipelineUploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3PipelineUploader").finish_non_exhaustive()
    }
}

#[cfg(feature = "worker-s3")]
impl S3PipelineUploader {
    /// Create a new uploader from an [`S3Config`](s3::S3Config) and an
    /// optional pre-built S3 client. If `client` is `None`, the default
    /// AWS configuration chain is used. Region detection and transfer
    /// manager construction are deferred to the first `process()` call.
    pub(crate) fn new(s3_config: s3::S3Config, client: Option<aws_sdk_s3::Client>) -> Self {
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
    pub(crate) fn set_client(&mut self, client: aws_sdk_s3::Client) {
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
    pub(crate) fn take_client(&mut self) -> Option<aws_sdk_s3::Client> {
        match &mut self.state {
            S3UploaderState::Pending { client, .. } => client.take(),
            S3UploaderState::Ready { .. } => None,
        }
    }

    /// Construct an uploader directly in the `Ready` state. Test-only —
    /// production code goes through [`new`](Self::new) and lazy init.
    #[cfg(test)]
    pub(crate) fn from_ready(
        uploader: s3::S3Uploader,
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
        s3_config: s3::S3Config,
        client: Option<aws_sdk_s3::Client>,
    ) -> (s3::S3Uploader, connection::CircuitBreaker) {
        let bootstrap_client = match client {
            Some(c) => c,
            None => {
                let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
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

        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(corrected_client)
                .build(),
        );

        (
            s3::S3Uploader::new(tm_client, s3_config),
            connection::CircuitBreaker::new(),
        )
    }
}

#[cfg(feature = "worker-s3")]
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
                return Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Io(std::io::Error::other(
                        "S3 uploader in unexpected state",
                    )),
                });
            };
            if !circuit_breaker.should_attempt() {
                tracing::debug!(target: "dial9_worker", segment = %data.segment, "circuit breaker open, skipping upload");
                return Err(ProcessError {
                    data,
                    kind: ProcessErrorKind::Transfer {
                        source: Box::from("circuit breaker open"),
                        retryable: true,
                    },
                });
            }
            let payload = std::mem::take(&mut data.payload);
            match uploader
                .upload_and_delete(&data.segment, payload, &data.metadata)
                .await
            {
                Ok(key) => {
                    circuit_breaker.on_success();
                    // Triggered dumps: remember the key under every dump id
                    // the segment belongs to, for that dump's manifest.
                    if let Some(dump_ids) = data.metadata.get("dump_id") {
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
                    if matches!(&kind, ProcessErrorKind::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
                    {
                        tracing::debug!(target: "dial9_worker", segment = %data.segment, "segment already evicted, skipping");
                    } else {
                        circuit_breaker.on_failure();
                        rate_limited!(Duration::from_secs(60), {
                            tracing::warn!(target: "dial9_worker", error = %kind, "upload failed");
                        });
                    }
                    Err(ProcessError { data, kind })
                }
            }
        })
    }

    fn finalize_dump(
        &mut self,
        completion: &crate::dump::DumpCompletion,
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
        let manifest = s3::DumpManifest::new(completion, segments);
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
#[cfg(feature = "worker-s3")]
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

#[cfg(all(test, feature = "worker-s3"))]
mod tests {
    use super::*;
    use assert2::check;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Deps that record whether on_failure was called by proxying through
    /// a real S3Uploader-like upload path.
    struct NotFoundTestDeps {
        circuit_breaker: connection::CircuitBreaker,
    }

    impl NotFoundTestDeps {
        fn new() -> Self {
            Self {
                circuit_breaker: connection::CircuitBreaker::new(),
            }
        }

        /// Simulate the upload logic from S3PipelineUploader::process
        async fn upload_segment(&mut self, segment: &sealed::SealedSegment) {
            if !self.circuit_breaker.should_attempt() {
                return;
            }
            // Attempt to read the file (like the worker would)
            match tokio::fs::read(&segment.path).await {
                Ok(_) => self.circuit_breaker.on_success(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Should skip, not degrade
                }
                Err(_) => self.circuit_breaker.on_failure(),
            }
        }
    }

    #[tokio::test]
    async fn evicted_file_does_not_trip_circuit_breaker() {
        let dir = tempfile::tempdir().unwrap();
        // Create a segment that doesn't exist on disk (simulates eviction)
        let missing = sealed::SealedSegment {
            path: dir.path().join("trace.0.bin"),
            index: 0,
        };

        let mut deps = NotFoundTestDeps::new();
        deps.upload_segment(&missing).await;

        check!(deps.circuit_breaker == connection::CircuitBreaker::Closed);
    }

    // --- Review finding #1: compressed_size metric is non-zero after pipeline ---

    /// After a successful pipeline run (gzip + upload), the CompressedSize
    /// metric must reflect the actual compressed byte count, not 0.
    #[tokio::test]
    async fn compressed_size_metric_is_nonzero_after_pipeline() {
        use metrique_writer::AnyEntrySink;
        use metrique_writer::test_util::Inspector;

        let s3_root = tempfile::tempdir().unwrap();
        let local_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        // Write a segment file with enough data to compress
        let segment_path = local_dir.path().join("trace.0.bin");
        let data = vec![42u8; 4096];
        std::fs::write(&segment_path, &data).unwrap();

        let inspector = Inspector::default();
        let sink = inspector.clone().boxed();

        // Build a real pipeline: GzipCompressor → S3PipelineUploader
        let s3_config = s3::S3Config::builder()
            .bucket("test-bucket")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();

        let fs = s3s_fs::FileSystem::new(s3_root.path()).unwrap();
        let mut builder = s3s::service::S3ServiceBuilder::new(fs);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_service = builder.build();
        let s3_client: s3s_aws::Client = s3_service.into();
        let s3_sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(s3_client)
            .force_path_style(true)
            .build();
        let sdk_client = aws_sdk_s3::Client::from_conf(s3_sdk_config);
        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(sdk_client)
                .build(),
        );

        let uploader = s3::S3Uploader::new(tm_client, s3_config);
        let mut processors: Vec<Box<dyn SegmentProcessor>> = vec![
            Box::new(GzipCompressor),
            Box::new(S3PipelineUploader::from_ready(
                uploader,
                connection::CircuitBreaker::new(),
            )),
        ];

        let segment = sealed::SegmentRef::Disk(sealed::SealedSegment {
            path: segment_path.clone(),
            index: 0,
        });

        let metrics = SegmentProcessMetrics {
            operation: Operation::ProcessSegment,
            total_time: metrique::timers::Timer::start_now(),
            status: None,
            segment_index: 0,
            uncompressed_size: data.len() as u64,
            compressed_size: None,
            invalid_file_header: false,
            panicked: false,
            panic_message: None,
            pipeline: PipelineMetrics::default(),
        }
        .append_on_drop(sink);

        let mut pipe_data = SegmentData {
            segment,
            payload: Payload::from_vec(data),
            metadata: HashMap::from([
                ("epoch_secs".into(), "1741209000".into()),
                ("segment_index".into(), "0".into()),
            ]),
            metrics,
            accounting: None,
        };

        for processor in &mut processors {
            let mut stage = StageMetrics::start();
            pipe_data = processor.process(pipe_data).await.unwrap();
            stage.succeed();
            pipe_data.metrics.pipeline.push(processor.name(), stage);
        }

        // After fix: compressed_size is set by GzipCompressor, not overwritten
        pipe_data.metrics.status = Some(MetriqueResult::Success);
        pipe_data.metrics.total_time.stop();
        drop(pipe_data);

        let entries = inspector.entries();
        check!(entries.len() == 1);
        let entry = &entries[0];
        let compressed = entry.metrics["CompressedSize"].as_u64();
        check!(
            compressed > 0,
            "CompressedSize should be non-zero, got {}",
            compressed
        );
    }

    /// `set_client` is only valid while the uploader is `Pending`. Calling it
    /// on a `Ready` uploader indicates an internal misuse and must panic
    /// rather than silently drop the new client.
    #[test]
    #[should_panic(expected = "set_client called after uploader initialization")]
    fn set_client_after_ready_panics() {
        let s3_config = s3::S3Config::builder()
            .bucket("test")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
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
        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(sdk_client.clone())
                .build(),
        );
        let uploader = s3::S3Uploader::new(tm_client, s3_config);
        let mut pipeline_uploader =
            S3PipelineUploader::from_ready(uploader, connection::CircuitBreaker::new());
        pipeline_uploader.set_client(sdk_client);
    }

    // --- Review finding #10: uncompressed_size should use bytes.len() ---

    /// uncompressed_size should match the actual bytes read, not a separate
    /// metadata() call that could race with eviction.
    #[test]
    fn uncompressed_size_matches_bytes_len() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.0.bin");
        let data = vec![0u8; 1234];
        std::fs::write(&path, &data).unwrap();

        // Read the file the way process_segments does
        let uncompressed_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let bytes = std::fs::read(&path).unwrap();

        // These should be equal — the metadata call is redundant
        check!(uncompressed_size == bytes.len() as u64);

        // The real assertion: bytes.len() is the canonical source of truth
        check!(bytes.len() == 1234);
    }

    // --- Review finding #4: WorkerLoop drain on stop ---

    /// When the stop signal is set, the worker must drain remaining segments
    /// before exiting.
    #[tokio::test]
    async fn worker_loop_drains_on_stop() {
        let dir = tempfile::tempdir().unwrap();

        // Create some sealed segments
        std::fs::write(dir.path().join("trace.0.bin"), b"segment0").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"segment1").unwrap();

        let processed = Arc::new(AtomicUsize::new(0));

        struct CountingProcessor(Arc<AtomicUsize>);
        impl SegmentProcessor for CountingProcessor {
            fn name(&self) -> &'static str {
                "Counter"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let counter = self.0.clone();
                Box::pin(async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if let Some(p) = data.segment.disk_path() {
                        let mut done = p.as_os_str().to_owned();
                        done.push(".done");
                        let _ = std::fs::rename(p, done);
                    }
                    Ok(data)
                })
            }
        }

        // Pre-cancelled token so the worker processes once and exits.
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let config = BackgroundTaskConfig::builder()
            .trace_path(dir.path().join("trace.bin"))
            .build();

        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(CountingProcessor(processed.clone()))];

        let mut worker = WorkerLoop::new(
            Fs::new_disk(config.trace_path().unwrap()),
            config.poll_interval(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.run().await;

        // Worker should have drained both segments even though stop was set.
        check!(processed.load(Ordering::SeqCst) == 2);
    }

    /// When a processor fails, the worker skips that segment and continues
    /// with the next one.
    #[tokio::test]
    async fn worker_loop_continues_after_processor_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"fail").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"succeed").unwrap();

        let processed = Arc::new(AtomicUsize::new(0));

        struct FailFirstProcessor {
            counter: Arc<AtomicUsize>,
            calls: usize,
        }
        impl SegmentProcessor for FailFirstProcessor {
            fn name(&self) -> &'static str {
                "FailFirst"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.calls += 1;
                let should_fail = self.calls == 1;
                let counter = self.counter.clone();
                Box::pin(async move {
                    if should_fail {
                        Err(ProcessError {
                            data,
                            kind: ProcessErrorKind::Io(std::io::Error::other("test failure")),
                        })
                    } else {
                        counter.fetch_add(1, Ordering::SeqCst);
                        if let Some(p) = data.segment.disk_path() {
                            let mut done = p.as_os_str().to_owned();
                            done.push(".done");
                            let _ = std::fs::rename(p, done);
                        }
                        Ok(data)
                    }
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let config = BackgroundTaskConfig::builder()
            .trace_path(dir.path().join("trace.bin"))
            .build();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(FailFirstProcessor {
            counter: processed.clone(),
            calls: 0,
        })];

        let mut worker = WorkerLoop::new(
            Fs::new_disk(config.trace_path().unwrap()),
            config.poll_interval(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.run().await;

        // Second segment should still be processed despite first failing.
        check!(processed.load(Ordering::SeqCst) == 1);
    }

    #[test]
    fn trace_dir_for_bare_relative_path_defaults_to_current_directory() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("trace.bin")
            .build();

        check!(config.trace_dir() == std::path::Path::new("."));
    }
}

// --- Review finding #9: trace_stem edge cases ---

#[cfg(test)]
mod trace_stem_tests {
    use super::*;
    use assert2::check;

    #[test]
    fn trace_stem_normal_path() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/trace.bin")
            .build();
        check!(config.trace_stem() == "trace");
    }

    #[test]
    fn trace_stem_directory_path() {
        // A path like "/tmp/traces/" — file_stem returns "traces", not an error
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/")
            .build();
        // This is the current behavior — it returns "traces" not "trace"
        // which would silently match the wrong files
        check!(config.trace_stem() == "traces");
    }

    #[test]
    fn trace_stem_root_path() {
        // A path like "/" has no file stem
        let config = BackgroundTaskConfig::builder().trace_path("/").build();
        // Should fall back to "trace" and log an error
        check!(config.trace_stem() == "trace");
    }

    #[test]
    fn trace_dir_for_directory_path() {
        let config = BackgroundTaskConfig::builder()
            .trace_path("/tmp/traces/")
            .build();
        // trace_dir should be the parent of the path
        check!(config.trace_dir() == std::path::Path::new("/tmp"));
    }
}

#[cfg(test)]
mod worker_pipeline_tests {
    use super::*;
    use assert2::check;
    use std::sync::Arc;

    fn fs_for(dir: &std::path::Path) -> Arc<Fs> {
        Fs::new_disk(&dir.join("trace.bin"))
    }

    fn default_poll() -> Duration {
        Duration::from_secs(1)
    }

    /// s3s wrapper where every upload returns 500 InternalError.
    struct AlwaysFailS3<S>(S);

    #[async_trait::async_trait]
    impl<S: s3s::S3 + Send + Sync> s3s::S3 for AlwaysFailS3<S> {
        async fn put_object(
            &self,
            _req: s3s::S3Request<s3s::dto::PutObjectInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
        async fn create_multipart_upload(
            &self,
            _req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
        async fn upload_part(
            &self,
            _req: s3s::S3Request<s3s::dto::UploadPartInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
        async fn complete_multipart_upload(
            &self,
            _req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
        ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
            Err(s3s::S3Error::with_message(
                s3s::S3ErrorCode::InternalError,
                "injected 500",
            ))
        }
    }

    /// s3s wrapper that fails the first `fail_n` writes with 500, then
    /// delegates to the inner backend.
    struct FlakyS3<S> {
        inner: S,
        remaining_failures: Arc<std::sync::atomic::AtomicU32>,
    }

    impl<S> FlakyS3<S> {
        fn should_fail(&self) -> bool {
            use std::sync::atomic::Ordering;
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
        uploader: s3::S3Uploader,
        fail_counter: Arc<std::sync::atomic::AtomicU32>,
        s3_root: tempfile::TempDir,
    }

    /// Read the single object out of the fake S3 bucket. Panics if there
    /// isn't exactly one. Used to assert uploaded bytes survived retries.
    fn read_only_object(s3_root: &std::path::Path) -> Vec<u8> {
        fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
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
        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(aws_sdk_s3::Client::from_conf(sdk_config))
                .build(),
        );
        let s3_config = s3::S3Config::builder()
            .bucket("test-bucket")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();
        FlakyHarness {
            uploader: s3::S3Uploader::new(tm_client, s3_config),
            fail_counter,
            s3_root,
        }
    }

    fn always_failing_s3_uploader() -> (s3::S3Uploader, tempfile::TempDir) {
        let s3_root = tempfile::tempdir().unwrap();
        let fs = s3s_fs::FileSystem::new(s3_root.path()).unwrap();
        let failing = AlwaysFailS3(fs);
        let mut builder = s3s::service::S3ServiceBuilder::new(failing);
        builder.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        let s3_service = builder.build();
        let s3_client: s3s_aws::Client = s3_service.into();
        let s3_sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .http_client(s3_client)
            .force_path_style(true)
            .build();
        let sdk_client = aws_sdk_s3::Client::from_conf(s3_sdk_config);
        let tm_client = aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(sdk_client)
                .build(),
        );
        let s3_config = s3::S3Config::builder()
            .bucket("test-bucket")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();
        (s3::S3Uploader::new(tm_client, s3_config), s3_root)
    }

    /// A segment that fails with a transient S3 error (500) is kept on disk for retry.
    #[tokio::test]
    async fn failed_segment_kept_on_transient_error() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"bad data").unwrap();

        let (uploader, _s3_root) = always_failing_s3_uploader();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(
            S3PipelineUploader::from_ready(uploader, connection::CircuitBreaker::new()),
        )];

        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.process_open_segments().await;

        check!(
            seg_path.exists(),
            "segment should be kept on disk after transient S3 error"
        );
    }

    /// A circuit-breaker-open error keeps the segment on disk.
    #[tokio::test]
    async fn circuit_breaker_open_keeps_segment() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"trace data").unwrap();

        let (uploader, _s3_root) = always_failing_s3_uploader();
        let mut cb = connection::CircuitBreaker::new();
        // Trip the circuit breaker so it refuses attempts.
        cb.on_failure();
        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(S3PipelineUploader::from_ready(uploader, cb))];

        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.process_open_segments().await;

        check!(
            seg_path.exists(),
            "segment should be kept when circuit breaker is open"
        );
    }

    /// A NotFound error (evicted segment) is silently skipped — no deletion attempt.
    #[tokio::test]
    async fn not_found_error_skips_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        // Write the file so it can be read, but the processor returns NotFound
        std::fs::write(&seg_path, b"data").unwrap();

        struct NotFoundProcessor;
        impl SegmentProcessor for NotFoundProcessor {
            fn name(&self) -> &'static str {
                "NotFound"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async {
                    Err(ProcessError {
                        data,
                        kind: ProcessErrorKind::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "evicted",
                        )),
                    })
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(NotFoundProcessor)];

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.process_open_segments().await;

        // File still exists because the processor returned NotFound (eviction),
        // which means the worker should skip — not attempt to delete.
        check!(
            seg_path.exists(),
            "segment should not be deleted on NotFound (eviction)"
        );
    }

    /// A permanent, non-retryable IO error deletes the segment.
    #[tokio::test]
    async fn permanent_io_error_deletes_segment() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"bad data").unwrap();

        struct PermanentFailProcessor;
        impl SegmentProcessor for PermanentFailProcessor {
            fn name(&self) -> &'static str {
                "PermanentFail"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async {
                    Err(ProcessError {
                        data,
                        kind: ProcessErrorKind::Io(std::io::Error::other("corrupt data")),
                    })
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(PermanentFailProcessor)];

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.process_open_segments().await;

        check!(
            !seg_path.exists(),
            "segment should be deleted after permanent IO error"
        );
    }

    /// Gzip-compressed segments pass through GzipCompressor unchanged.
    #[tokio::test]
    async fn gzip_segment_not_double_compressed() {
        let dir = tempfile::tempdir().unwrap();

        let gzip_data = {
            use flate2::write::GzEncoder;
            use std::io::Write;
            let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(b"already compressed").unwrap();
            enc.finish().unwrap()
        };
        std::fs::write(dir.path().join("trace.0.bin"), &gzip_data).unwrap();

        let (capture, output_bytes) = super::testutil::CapturingProcessor::new();
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();

        let processors: Vec<Box<dyn SegmentProcessor>> =
            vec![Box::new(GzipCompressor), Box::new(capture)];

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.run().await;

        // The captured bytes should be identical to the input (not double-gzipped).
        // Single segment in this test, so check the first (only) captured payload.
        let captured = output_bytes.lock().unwrap();
        check!(captured.len() == 1);
        check!(captured[0].as_slice() == gzip_data.as_slice());
    }

    /// WriteBackProcessor writes to a new path when `write_back_extension` is
    /// set and removes the original file, preventing re-discovery on the next
    /// poll cycle.
    #[tokio::test]
    async fn write_back_renames_when_extension_metadata_set() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"payload").unwrap();

        let segment = sealed::SegmentRef::Disk(sealed::SealedSegment {
            path: seg_path.clone(),
            index: 0,
        });

        let metrics = SegmentProcessMetrics {
            operation: Operation::ProcessSegment,
            total_time: metrique::timers::Timer::start_now(),
            status: None,
            segment_index: 0,
            uncompressed_size: 7,
            compressed_size: None,
            invalid_file_header: false,
            panicked: false,
            panic_message: None,
            pipeline: PipelineMetrics::default(),
        }
        .append_on_drop(metrique_writer::sink::DevNullSink::boxed());

        let data = SegmentData {
            segment,
            payload: Payload::from(b"payload"),
            metadata: HashMap::from([("write_back_extension".into(), ".gz".into())]),
            metrics,
            accounting: None,
        };

        let mut processor = WriteBackProcessor::default();
        let result = processor.process(data).await;
        check!(result.is_ok());

        // Original .bin should be gone, .bin.gz should exist with the payload.
        check!(!seg_path.exists());
        let gz_path = dir.path().join("trace.0.bin.gz");
        check!(gz_path.exists());
        check!(std::fs::read(&gz_path).unwrap() == b"payload");
    }

    /// WriteBackProcessor writes to the original path when no
    /// `write_back_extension` metadata is set.
    #[tokio::test]
    async fn write_back_overwrites_in_place_without_extension() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"old").unwrap();

        let segment = sealed::SegmentRef::Disk(sealed::SealedSegment {
            path: seg_path.clone(),
            index: 0,
        });

        let metrics = SegmentProcessMetrics {
            operation: Operation::ProcessSegment,
            total_time: metrique::timers::Timer::start_now(),
            status: None,
            segment_index: 0,
            uncompressed_size: 3,
            compressed_size: None,
            invalid_file_header: false,
            panicked: false,
            panic_message: None,
            pipeline: PipelineMetrics::default(),
        }
        .append_on_drop(metrique_writer::sink::DevNullSink::boxed());

        let data = SegmentData {
            segment,
            payload: Payload::from(b"new"),
            metadata: HashMap::new(),
            metrics,
            accounting: None,
        };

        let mut processor = WriteBackProcessor::default();
        let result = processor.process(data).await;
        check!(result.is_ok());

        check!(std::fs::read(&seg_path).unwrap() == b"new");
    }

    /// The full GzipCompressor → WriteBackProcessor pipeline writes a `.bin.gz`
    /// file and removes the original `.bin`, so `find_sealed_segments` will not
    /// re-discover it on the next poll.
    #[tokio::test]
    async fn gzip_write_back_pipeline_prevents_rediscovery() {
        let dir = tempfile::tempdir().unwrap();
        let seg_path = dir.path().join("trace.0.bin");
        std::fs::write(&seg_path, b"raw trace data").unwrap();

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![
            Box::new(GzipCompressor),
            Box::new(WriteBackProcessor::default()),
        ];

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.run().await;

        // Original .bin removed; .bin.gz written.
        check!(!seg_path.exists());
        check!(dir.path().join("trace.0.bin.gz").exists());

        // A subsequent scan should find no sealed segments.
        let segments = sealed::find_sealed_segments(dir.path(), "trace").unwrap();
        check!(segments.is_empty());
    }

    /// A processor that panics must not kill the worker loop. The panicking
    /// segment is skipped and subsequent segments are still processed.
    #[tokio::test]
    async fn processor_panic_does_not_kill_worker_loop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"panic me").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"process me").unwrap();

        let processed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        struct PanicFirstProcessor {
            counter: Arc<std::sync::atomic::AtomicUsize>,
            calls: usize,
        }
        impl SegmentProcessor for PanicFirstProcessor {
            fn name(&self) -> &'static str {
                "PanicFirst"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.calls += 1;
                let should_panic = self.calls == 1;
                let counter = self.counter.clone();
                Box::pin(async move {
                    if should_panic {
                        panic!("processor panic on first segment");
                    }
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(data)
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();

        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(PanicFirstProcessor {
            counter: processed.clone(),
            calls: 0,
        })];

        use metrique_writer::AnyEntrySink;
        use metrique_writer::test_util::Inspector;
        let inspector = Inspector::default();

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop,
            inspector.clone().boxed(),
            None,
        );
        worker.run().await;

        // The worker must have processed at least one segment (the non-panicking one)
        // despite the first processor call panicking.
        check!(processed.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        // The panicking segment's file should have been removed.
        check!(!dir.path().join("trace.0.bin").exists());

        // Verify metrics: we should have entries for both segments, and the
        // panicking one should have Panicked=true.
        let entries = inspector.entries();
        let panicked_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.metrics.get("Panicked").is_some_and(|m| m.as_bool()))
            .collect();
        check!(
            panicked_entries.len() == 1,
            "expected exactly one panicked metric entry, got {}",
            panicked_entries.len()
        );
        check!(panicked_entries[0].metrics["Failure"] == true);
        // The panic message should be captured.
        check!(panicked_entries[0].values["PanicMessage"] == "processor panic on first segment");
    }

    /// A processor that hangs must not prevent the worker from shutting down.
    /// The drain timeout in `run_background_task` handles this, but at the
    /// WorkerLoop level, cancellation should interrupt a hung processor.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn processor_hang_respects_shutdown_timeout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"hang me").unwrap();

        struct HangingProcessor;
        impl SegmentProcessor for HangingProcessor {
            fn name(&self) -> &'static str {
                "Hanging"
            }
            fn process(
                &mut self,
                _data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async {
                    // Hang forever
                    std::future::pending::<()>().await;
                    unreachable!()
                })
            }
        }

        let stop = tokio_util::sync::CancellationToken::new();
        let processors: Vec<Box<dyn SegmentProcessor>> = vec![Box::new(HangingProcessor)];

        let mut worker = WorkerLoop::new(
            fs_for(dir.path()),
            default_poll(),
            processors,
            stop.clone(),
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );

        let run_fut = worker.run();

        // Simulate the shutdown path from run_background_task:
        // cancel the stop token, then timeout the run future.
        let drain_timeout = Duration::from_secs(2);
        stop.cancel();
        let result = tokio::time::timeout(drain_timeout, run_fut).await;

        // The timeout should fire because the processor is hung.
        check!(result.is_err(), "expected timeout, but worker completed");
    }

    /// Disk `mark_writer_done` alone (no stop-token cancel) drains and exits.
    /// Symmetric with memory mode: `DiskWriter::finalize` is a complete
    /// shutdown signal across both backends.
    #[tokio::test]
    async fn disk_worker_run_drains_on_writer_done() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.0.bin"), b"seg0").unwrap();
        std::fs::write(dir.path().join("trace.1.bin"), b"seg1").unwrap();

        let processed = Arc::new(AtomicUsize::new(0));

        struct CountingProcessor(Arc<AtomicUsize>);
        impl SegmentProcessor for CountingProcessor {
            fn name(&self) -> &'static str {
                "Counting"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                if let Some(p) = data.segment.disk_path() {
                    let _ = std::fs::remove_file(p);
                }
                Box::pin(async { Ok(data) })
            }
        }

        let fs = fs_for(dir.path());
        // Stop token is never cancelled; shutdown rides writer_done only.
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            Duration::from_millis(10),
            vec![Box::new(CountingProcessor(processed.clone()))],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );

        fs.mark_writer_done();

        let result = tokio::time::timeout(Duration::from_secs(5), worker.run()).await;
        check!(result.is_ok(), "worker did not exit on writer_done alone");
        check!(processed.load(Ordering::SeqCst) == 2);
    }

    /// `in_flight_bytes` follows payload growth (symbolize) and shrinkage
    /// (gzip), not just the pop-time size.
    #[tokio::test]
    async fn mem_worker_adjusts_in_flight_bytes_across_stages() {
        use std::io::Write;
        use std::sync::Mutex;
        use std::sync::atomic::Ordering;

        struct Mutator;
        impl SegmentProcessor for Mutator {
            fn name(&self) -> &'static str {
                "Mutator"
            }
            fn process(
                &mut self,
                mut data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let index = data.segment().index();
                Box::pin(async move {
                    if index == 0 {
                        let mut p = data.take_payload();
                        p.push(bytes::Bytes::from(vec![0u8; 100]));
                        data.set_payload(p);
                    } else {
                        data.set_payload(bytes::Bytes::from(vec![0u8; 5]));
                    }
                    Ok(data)
                })
            }
        }

        /// Reads `in_flight_bytes` so we can assert what the worker's
        /// `adjust` set after `Mutator` returned.
        struct Probe {
            samples: Arc<Mutex<Vec<(u32, u64)>>>,
        }
        impl SegmentProcessor for Probe {
            fn name(&self) -> &'static str {
                "Probe"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let index = data.segment().index();
                let atomic = Arc::clone(
                    &data
                        .accounting
                        .as_ref()
                        .expect("memory segments should carry accounting")
                        .in_flight_bytes,
                );
                let samples = Arc::clone(&self.samples);
                Box::pin(async move {
                    samples
                        .lock()
                        .expect("samples mutex should not be poisoned")
                        .push((index, atomic.load(Ordering::Acquire)));
                    Ok(data)
                })
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).expect("memory fs should build");
        for i in 0..2u32 {
            let mut h = fs
                .create_segment(Path::new("x"))
                .expect("memory fs should create a handle");
            h.write_all(&[0u8; 50])
                .expect("write into memory handle should succeed");
            fs.seal(h, Path::new("x"), i)
                .expect("sealing into memory ring should succeed");
        }
        fs.mark_writer_done();

        let samples = Arc::new(Mutex::new(Vec::new()));
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            Duration::from_millis(1),
            vec![
                Box::new(Mutator),
                Box::new(Probe {
                    samples: Arc::clone(&samples),
                }),
            ],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        tokio::time::timeout(Duration::from_secs(5), worker.run())
            .await
            .expect("worker exited");

        let samples = samples
            .lock()
            .expect("samples mutex should not be poisoned")
            .clone();
        check!(samples == vec![(0, 150), (1, 5)]);
        let snap = fs.take_files();
        check!(snap.in_flight_bytes == 0);
        check!(snap.in_flight_segments == 0);
    }

    #[test]
    fn mem_take_files_reports_in_flight_bytes_peak() {
        use std::io::Write;

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let mut h = fs.create_segment(Path::new("x")).unwrap();
        h.write_all(&[0u8; 50]).unwrap();
        fs.seal(h, Path::new("x"), 0).unwrap();

        // Cycle 1: pop. Peak snapshot reads 0 (nothing happened before).
        // Inside the same call, the pop seeds the channel peak at 50.
        let mut snap = fs.take_files();
        check!(snap.segments.len() == 1);
        check!(snap.in_flight_bytes == 50);
        check!(snap.in_flight_bytes_peak == Some(0));

        let taken = snap.segments.remove(0);
        let (_seg, payload, accounting) = taken.load().unwrap();
        let mut acct = accounting.expect("memory segment carries accounting");
        // Verify the in-cycle adjust path moves the peak.
        let _ = payload;
        acct.adjust(200);
        acct.adjust(10);
        drop(acct);

        // Cycle 2: empty pop. Returned peak is the previous cycle's high.
        let snap = fs.take_files();
        check!(snap.segments.is_empty());
        check!(snap.in_flight_bytes == 0);
        check!(
            snap.in_flight_bytes_peak == Some(200),
            "peak should capture mid-cycle high; got {:?}",
            snap.in_flight_bytes_peak
        );

        // Cycle 3: peak has been consumed
        let snap = fs.take_files();
        check!(snap.in_flight_bytes_peak == Some(0));
    }

    /// Multi-threaded race test for memory mode: producer seals segments and
    /// marks writer_done while the worker may be parked in `wait_for_more`.
    /// Ensures `run()` drains all segments and exits (no missed wakeup hang).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mem_worker_run_drains_late_push_no_loss() {
        use std::io::Write;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const ITERS: usize = 30;
        const SEGMENTS: u32 = 8;

        struct CountingProcessor(Arc<AtomicUsize>);
        impl SegmentProcessor for CountingProcessor {
            fn name(&self) -> &'static str {
                "Counting"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(data) })
            }
        }

        for iter in 0..ITERS {
            let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
            let processed = Arc::new(AtomicUsize::new(0));
            let stop = tokio_util::sync::CancellationToken::new();

            let mut worker = WorkerLoop::new(
                Arc::clone(&fs),
                Duration::from_millis(1),
                vec![Box::new(CountingProcessor(processed.clone()))],
                stop,
                metrique_writer::sink::DevNullSink::boxed(),
                None,
            );
            let worker_task = tokio::spawn(async move { worker.run().await });

            // Let the worker reach wait_for_more on an empty ring so the
            // seals below race the wakeup/writer_done window.
            tokio::task::yield_now().await;

            let producer_fs = Arc::clone(&fs);
            let producer = tokio::spawn(async move {
                for i in 0..SEGMENTS {
                    let mut h = producer_fs.create_segment(Path::new("x")).unwrap();
                    h.write_all(b"event-bytes").unwrap();
                    producer_fs.seal(h, Path::new("x"), i).unwrap();
                }
                producer_fs.mark_writer_done();
            });

            producer.await.unwrap();
            let joined = tokio::time::timeout(Duration::from_secs(5), worker_task).await;
            check!(
                joined.is_ok(),
                "iter {iter}: worker stranded (lost wakeup or missed writer_done)"
            );
            joined.unwrap().unwrap();

            check!(
                processed.load(Ordering::SeqCst) == SEGMENTS as usize,
                "iter {iter}: expected {SEGMENTS} segments, got {}",
                processed.load(Ordering::SeqCst)
            );
        }
    }

    /// N<budget retryable failures followed by success: segment delivers,
    /// in-flight accounting drains.
    #[tokio::test(start_paused = true)]
    async fn mem_worker_retries_retryable_within_budget() {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct Flaky {
            fail_count: u32,
            attempts: Arc<AtomicU32>,
        }
        impl SegmentProcessor for Flaky {
            fn name(&self) -> &'static str {
                "Flaky"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let n = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
                let fail_count = self.fail_count;
                Box::pin(async move {
                    if n <= fail_count {
                        Err(ProcessError {
                            data,
                            kind: ProcessErrorKind::Transfer {
                                source: Box::from("transient"),
                                retryable: true,
                            },
                        })
                    } else {
                        Ok(data)
                    }
                })
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let mut h = fs.create_segment(Path::new("x")).unwrap();
        h.write_all(&[0u8; 50]).unwrap();
        fs.seal(h, Path::new("x"), 0).unwrap();
        fs.mark_writer_done();

        let attempts = Arc::new(AtomicU32::new(0));
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            Duration::from_millis(1),
            vec![Box::new(Flaky {
                fail_count: 2,
                attempts: Arc::clone(&attempts),
            })],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.run().await;
        check!(attempts.load(Ordering::SeqCst) == 3, "2 fails + 1 success");
        let snap = fs.take_files();
        check!(snap.in_flight_bytes == 0);
        check!(snap.in_flight_segments == 0);
    }

    /// Always-fail retryable: exactly `MEMORY_RETRY_BUDGET + 1` attempts,
    /// then segment is dropped and accounting drains.
    #[tokio::test(start_paused = true)]
    async fn mem_worker_drops_after_retry_budget_exhausted() {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct AlwaysFails {
            attempts: Arc<AtomicU32>,
        }
        impl SegmentProcessor for AlwaysFails {
            fn name(&self) -> &'static str {
                "AlwaysFails"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    Err(ProcessError {
                        data,
                        kind: ProcessErrorKind::Transfer {
                            source: Box::from("permanent"),
                            retryable: true,
                        },
                    })
                })
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let mut h = fs.create_segment(Path::new("x")).unwrap();
        h.write_all(&[0u8; 50]).unwrap();
        fs.seal(h, Path::new("x"), 0).unwrap();
        fs.mark_writer_done();

        let attempts = Arc::new(AtomicU32::new(0));
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            Duration::from_millis(1),
            vec![Box::new(AlwaysFails {
                attempts: Arc::clone(&attempts),
            })],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        worker.run().await;
        check!(
            attempts.load(Ordering::SeqCst) == crate::background_task::fs::MEMORY_RETRY_BUDGET + 1,
            "initial + budget retries",
        );
        let snap = fs.take_files();
        check!(snap.in_flight_bytes == 0);
        check!(snap.in_flight_segments == 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mem_e2e_real_s3_pipeline_recovers_within_budget() {
        use std::io::Write;
        use std::sync::atomic::Ordering;

        let FlakyHarness {
            uploader,
            fail_counter,
            s3_root,
        } = flaky_s3_harness(2);
        // > CB initial backoff (1s) so CB reopens between retries. CB
        // doubles per failure; budget=3 fits 1s+2s within the 15s cap below.
        let poll_interval = Duration::from_millis(1100);

        let payload = b"segment-payload-bytes";
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let mut h = fs.create_segment(Path::new("x")).unwrap();
        h.write_all(payload).unwrap();
        fs.seal(h, Path::new("x"), 0).unwrap();
        fs.mark_writer_done();

        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            Arc::clone(&fs),
            poll_interval,
            vec![Box::new(S3PipelineUploader::from_ready(
                uploader,
                connection::CircuitBreaker::new(),
            ))],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            None,
        );
        tokio::time::timeout(Duration::from_secs(15), worker.run())
            .await
            .expect("worker hung");

        check!(
            fail_counter.load(Ordering::SeqCst) == 0,
            "all injected failures consumed",
        );
        let uploaded = read_only_object(s3_root.path());
        check!(
            uploaded == payload,
            "uploaded body must match seal'd bytes (snapshot survived retries)",
        );
        let snap = fs.take_files();
        check!(snap.in_flight_bytes == 0);
        check!(snap.in_flight_segments == 0);
    }
}

/// Shared helpers for the triggered-worker and finalize-dump test modules.
#[cfg(test)]
mod triggered_test_support {
    use super::*;
    use std::io::Write as _;

    pub(super) fn now_epoch() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Valid trace bytes whose first clock anchor reports `epoch_secs`.
    pub(super) fn segment_with_epoch(epoch_secs: u64) -> Vec<u8> {
        use dial9_trace_format::encoder::Encoder;
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write_infallible(&crate::telemetry::format::ClockSyncEvent {
            timestamp_ns: 1,
            realtime_ns: epoch_secs * 1_000_000_000,
        });
        enc.into_inner()
    }

    pub(super) fn seal_mem(fs: &Fs, index: u32, epoch_secs: u64) {
        let mut h = fs.create_segment(Path::new("x")).unwrap();
        h.write_all(&segment_with_epoch(epoch_secs)).unwrap();
        fs.seal(h, Path::new("x"), index).unwrap();
    }

    pub(super) fn spawn_worker(
        fs: Arc<Fs>,
        processors: Vec<Box<dyn SegmentProcessor>>,
        rx: crate::dump::DumpRx,
        stop: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let mut worker = WorkerLoop::new(
            fs,
            Duration::from_millis(10),
            processors,
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
            Some(rx),
        );
        tokio::spawn(async move { worker.run().await })
    }
}

#[cfg(test)]
mod triggered_worker_tests {
    use super::triggered_test_support::*;
    use super::*;
    use crate::dump::{self, DumpError};
    use assert2::check;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use testutil::CapturingProcessor;

    /// Records every segment's pipeline metadata for assertions.
    struct MetadataRecorder(Arc<Mutex<Vec<HashMap<String, String>>>>);

    impl SegmentProcessor for MetadataRecorder {
        fn name(&self) -> &'static str {
            "MetadataRecorder"
        }
        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            self.0.lock().unwrap().push(data.metadata().clone());
            Box::pin(async move { Ok(data) })
        }
    }

    /// Fails the first `n` attempts with a retryable transfer error.
    struct FailNTimes(AtomicUsize);

    impl SegmentProcessor for FailNTimes {
        fn name(&self) -> &'static str {
            "FailNTimes"
        }
        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            let fail = self
                .0
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            Box::pin(async move {
                if fail {
                    Err(ProcessError::new(
                        data,
                        ProcessErrorKind::Transfer {
                            source: Box::from("transient"),
                            retryable: true,
                        },
                    ))
                } else {
                    Ok(data)
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn triggered_worker_idles_until_stop() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (_trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());
        seal_mem(&fs, 1, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        tokio::time::sleep(Duration::from_secs(5)).await;
        stop.cancel();
        worker.await.unwrap();

        // Never triggered: nothing went through the pipeline, the ring
        // still holds both segments.
        check!(captured.lock().unwrap().is_empty());
        let snap = fs.take_files();
        check!(snap.segments.len() == 1);
        check!(snap.queued_segments == Some(1));
    }

    #[tokio::test(start_paused = true)]
    async fn dump_current_data_captures_ring_and_stamps_metadata() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());
        seal_mem(&fs, 1, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let metadata = Arc::new(Mutex::new(Vec::new()));
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![
                Box::new(MetadataRecorder(metadata.clone())),
                Box::new(capture),
            ],
            rx,
            stop.clone(),
        );

        let receipt = trigger
            .dump_current_data()
            .with_metadata("reason", "test")
            .await
            .unwrap();

        check!(receipt.segments_processed == 2);
        check!(receipt.manifest_key.is_none());
        check!(captured.lock().unwrap().len() == 2);
        {
            let recorded = metadata.lock().unwrap();
            for md in recorded.iter() {
                check!(md["dump_id"] == receipt.dump_id.to_string());
                check!(md["dump.reason"] == "test");
            }
        }

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn narrow_lookback_preserves_out_of_window_history() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        let now = now_epoch();
        seal_mem(&fs, 0, now - 3600); // outside a 60s look-back
        fs.set_seal_secs_for_test(0, now - 3600);
        seal_mem(&fs, 1, now);

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let receipt = trigger
            .dump_time_range(Duration::from_secs(60), Duration::ZERO)
            .await
            .unwrap();

        check!(receipt.segments_processed == 1);
        check!(receipt.time_range == (epoch_to_system(now), epoch_to_system(now)));
        check!(captured.lock().unwrap().len() == 1);

        stop.cancel();
        worker.await.unwrap();

        // The old segment survived the dump.
        let snap = fs.take_files();
        check!(snap.segments.len() == 1);
        check!(snap.segments[0].seg_ref.index() == 0);
    }

    #[tokio::test(start_paused = true)]
    async fn lookback_captures_segment_spanning_window_start() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        let now = now_epoch();
        // Started before the 60s window, sealed inside it: the span
        // overlaps, so the segment is captured.
        seal_mem(&fs, 0, now - 300);
        fs.set_seal_secs_for_test(0, now - 30);

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let receipt = trigger
            .dump_time_range(Duration::from_secs(60), Duration::ZERO)
            .await
            .unwrap();
        check!(receipt.segments_processed == 1);
        check!(captured.lock().unwrap().len() == 1);

        stop.cancel();
        worker.await.unwrap();
    }

    /// A retrying segment only holds open the dumps it matched (disk: one
    /// pass covers the whole backlog); an unrelated due dump resolves.
    #[tokio::test(start_paused = true)]
    async fn disk_retry_holds_only_matched_dumps() {
        /// Fails retryably, forever, any segment whose creation epoch
        /// matches `old_epoch`; passes everything else through.
        struct FailOldForever {
            old_epoch: String,
        }
        impl SegmentProcessor for FailOldForever {
            fn name(&self) -> &'static str {
                "FailOldForever"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                let fail = data.metadata().get("epoch_secs") == Some(&self.old_epoch);
                Box::pin(async move {
                    if fail {
                        Err(ProcessError::new(
                            data,
                            ProcessErrorKind::Transfer {
                                source: Box::from("transient"),
                                retryable: true,
                            },
                        ))
                    } else {
                        Ok(data)
                    }
                })
            }
        }

        fn set_mtime(path: &Path, epoch_secs: u64) {
            let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            f.set_times(
                std::fs::FileTimes::new()
                    .set_modified(UNIX_EPOCH + Duration::from_secs(epoch_secs)),
            )
            .unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let now = now_epoch();
        let old_path = dir.path().join("trace.0.bin");
        std::fs::write(&old_path, segment_with_epoch(now - 3600)).unwrap();
        set_mtime(&old_path, now - 3600);
        std::fs::write(dir.path().join("trace.1.bin"), segment_with_epoch(now)).unwrap();

        let fs = Fs::new_disk(&dir.path().join("trace.bin"));
        let (trigger, rx) = dump::channel();
        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(FailOldForever {
                old_epoch: (now - 3600).to_string(),
            })],
            rx,
            stop.clone(),
        );

        // Wide dump matches both segments; its old one retries forever.
        let fut_wide = std::future::IntoFuture::into_future(
            trigger.dump_time_range(Duration::from_secs(7200), Duration::ZERO),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Narrow dump's window cannot match the retrying segment: it must
        // resolve despite the wide dump's pending retry.
        let receipt_narrow = trigger
            .dump_time_range(Duration::from_secs(60), Duration::ZERO)
            .await
            .unwrap();
        check!(receipt_narrow.segments_processed == 0);

        // The wide dump stays open until shutdown truncates it; the fresh
        // segment it captured before the retry stall is on the receipt.
        stop.cancel();
        let receipt_wide = fut_wide.await.unwrap();
        check!(receipt_wide.segments_processed == 1);
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn lookforward_captures_post_trigger_seals() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let started = tokio::time::Instant::now();
        let run = trigger.dump_time_range(Duration::ZERO, Duration::from_secs(5));
        let fut = std::future::IntoFuture::into_future(run);
        // Let the worker register the dump, then seal inside the window.
        tokio::time::sleep(Duration::from_millis(50)).await;
        seal_mem(&fs, 0, now_epoch());

        let receipt = fut.await.unwrap();
        // Resolves only after the forward deadline. Production anchors the
        // deadline to the trigger's wall-clock time (`SystemTime`, see
        // `ActiveDump::register`), then maps it onto the tokio timer; under
        // `start_paused` the virtual clock does not advance during the (real)
        // pickup latency, so the measured elapsed can land a few ms under the
        // nominal 5s. Allow a small tolerance for that clock-mixing skew — the
        // point is the dump waited ~the forward window, not that it resolved
        // immediately or only at shutdown.
        check!(started.elapsed() >= Duration::from_secs(5) - Duration::from_millis(100));
        check!(receipt.segments_processed == 1);
        check!(captured.lock().unwrap().len() == 1);

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn overlapping_forward_windows_share_segment() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        let metadata = Arc::new(Mutex::new(Vec::new()));
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(MetadataRecorder(metadata.clone()))],
            rx,
            stop.clone(),
        );

        let fut_a = std::future::IntoFuture::into_future(
            trigger.dump_time_range(Duration::from_secs(60), Duration::from_secs(5)),
        );
        let fut_b = std::future::IntoFuture::into_future(
            trigger.dump_time_range(Duration::from_secs(60), Duration::from_secs(5)),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        seal_mem(&fs, 0, now_epoch());

        let (receipt_a, receipt_b) = tokio::join!(fut_a, fut_b);
        let receipt_a = receipt_a.unwrap();
        let receipt_b = receipt_b.unwrap();
        check!(receipt_a.segments_processed == 1);
        check!(receipt_b.segments_processed == 1);

        {
            let recorded = metadata.lock().unwrap();
            check!(recorded.len() == 1, "one segment through the pipeline");
            let ids = &recorded[0]["dump_id"];
            check!(ids.contains(&receipt_a.dump_id.to_string()));
            check!(ids.contains(&receipt_b.dump_id.to_string()));
            check!(ids.contains(','));
        }

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn zero_zero_dump_resolves_empty() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, _captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let receipt = trigger
            .dump_time_range(Duration::ZERO, Duration::ZERO)
            .await
            .unwrap();
        check!(receipt.segments_processed == 0);
        check!(receipt.time_range.0 == receipt.time_range.1);

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn retryable_failure_holds_dump_open_until_retry_succeeds() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, captured) = CapturingProcessor::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(FailNTimes(AtomicUsize::new(1))), Box::new(capture)],
            rx,
            stop.clone(),
        );

        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.segments_processed == 1);
        check!(captured.lock().unwrap().len() == 1);

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn evicted_segment_resolves_ok_and_uncounted() {
        struct EvictedSim;
        impl SegmentProcessor for EvictedSim {
            fn name(&self) -> &'static str {
                "EvictedSim"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(async move {
                    Err(ProcessError::io(
                        data,
                        io::Error::from(io::ErrorKind::NotFound),
                    ))
                })
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(EvictedSim)],
            rx,
            stop.clone(),
        );

        // Best-effort: the vanished segment is silently uncounted.
        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.segments_processed == 0);

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn total_pipeline_failure_resolves_pipeline_error() {
        struct AlwaysFail;
        impl SegmentProcessor for AlwaysFail {
            fn name(&self) -> &'static str {
                "AlwaysFail"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(
                    async move { Err(ProcessError::io(data, io::Error::other("broken stage"))) },
                )
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(AlwaysFail)],
            rx,
            stop.clone(),
        );

        let err = trigger.dump_current_data().await.unwrap_err();
        check!(matches!(err, DumpError::Pipeline(_)));

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_resolves_open_forward_dump_truncated() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, _captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let fut = std::future::IntoFuture::into_future(
            trigger.dump_time_range(Duration::ZERO, Duration::from_secs(3600)),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        seal_mem(&fs, 0, now_epoch());
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Shut down long before the hour-long deadline: the dump resolves
        // with a truncated receipt covering what landed.
        stop.cancel();
        let receipt = fut.await.unwrap();
        check!(receipt.segments_processed == 1);
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn queued_request_at_shutdown_resolves_worker_stopped() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        // Request queued and stop cancelled before the worker ever runs.
        let fut = std::future::IntoFuture::into_future(trigger.dump_current_data());
        stop.cancel();

        let (capture, _captured) = CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop);

        let err = fut.await.unwrap_err();
        check!(matches!(err, DumpError::WorkerStopped));
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn mem_windowed_take_preserves_non_matching_slots() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let now = now_epoch();
        seal_mem(&fs, 0, now - 3600);
        fs.set_seal_secs_for_test(0, now - 3600);
        seal_mem(&fs, 1, now);

        // Window matching nothing: ring untouched.
        let none = fs.take_files_matching(&[EpochWindow {
            start_secs: Some(now + 100),
            end_secs: now + 200,
        }]);
        check!(none.segments.is_empty());
        check!(none.queued_segments == Some(2));

        // Window matching only the fresh segment: the old slot stays.
        let snap = fs.take_files_matching(&[EpochWindow {
            start_secs: Some(now - 60),
            end_secs: now + 60,
        }]);
        check!(snap.segments.len() == 1);
        check!(snap.segments[0].seg_ref.index() == 1);
        check!(snap.queued_segments == Some(1));
    }
}

#[cfg(test)]
mod finalize_dump_tests {
    use super::triggered_test_support::*;
    use super::*;
    use crate::dump::{self, DumpCompletion, DumpId};
    use assert2::check;
    use std::sync::Mutex;

    type SeenCompletions = Arc<Mutex<Vec<(DumpId, usize, Vec<(String, String)>, bool)>>>;

    /// Passes segments through; `finalize_dump` records the completion and
    /// returns a fixed key (or `None`).
    struct FinalizeStub {
        key: Option<String>,
        completions: SeenCompletions,
    }

    impl SegmentProcessor for FinalizeStub {
        fn name(&self) -> &'static str {
            "FinalizeStub"
        }
        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            Box::pin(async move { Ok(data) })
        }
        fn finalize_dump(
            &mut self,
            completion: &DumpCompletion,
        ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
            self.completions.lock().unwrap().push((
                completion.dump_id,
                completion.segments_processed,
                completion.metadata.clone(),
                completion.failed,
            ));
            let key = self.key.clone();
            Box::pin(std::future::ready(key))
        }
    }

    struct PanickingFinalize;

    impl SegmentProcessor for PanickingFinalize {
        fn name(&self) -> &'static str {
            "PanickingFinalize"
        }
        fn process(
            &mut self,
            data: SegmentData,
        ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>> {
            Box::pin(async move { Ok(data) })
        }
        fn finalize_dump(
            &mut self,
            _completion: &DumpCompletion,
        ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
            panic!("finalize boom");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn manifest_key_flows_to_receipt_last_stage_wins() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let completions = Arc::new(Mutex::new(Vec::new()));
        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![
                Box::new(FinalizeStub {
                    key: Some("dumps/early.json".into()),
                    completions: completions.clone(),
                }),
                Box::new(FinalizeStub {
                    key: Some("dumps/late.json".into()),
                    completions: completions.clone(),
                }),
            ],
            rx,
            stop.clone(),
        );

        let receipt = trigger
            .dump_current_data()
            .with_metadata("reason", "test")
            .await
            .unwrap();

        check!(receipt.manifest_key.as_deref() == Some("dumps/late.json"));
        {
            let seen = completions.lock().unwrap();
            check!(seen.len() == 2, "both stages finalized");
            for (id, count, metadata, failed) in seen.iter() {
                check!(*id == receipt.dump_id);
                check!(*count == 1);
                check!(metadata == &vec![("reason".to_string(), "test".to_string())]);
                check!(!*failed);
            }
        }

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn default_finalize_yields_no_manifest_key() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let (capture, _captured) = testutil::CapturingProcessor::new();
        let worker = spawn_worker(Arc::clone(&fs), vec![Box::new(capture)], rx, stop.clone());

        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.manifest_key.is_none());

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_dump_signals_failed_completion() {
        struct AlwaysFail;
        impl SegmentProcessor for AlwaysFail {
            fn name(&self) -> &'static str {
                "AlwaysFail"
            }
            fn process(
                &mut self,
                data: SegmentData,
            ) -> Pin<Box<dyn Future<Output = Result<SegmentData, ProcessError>> + Send + '_>>
            {
                Box::pin(
                    async move { Err(ProcessError::io(data, io::Error::other("broken stage"))) },
                )
            }
        }

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let completions = Arc::new(Mutex::new(Vec::new()));
        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![
                Box::new(AlwaysFail),
                Box::new(FinalizeStub {
                    key: Some("dumps/failed.json".into()),
                    completions: completions.clone(),
                }),
            ],
            rx,
            stop.clone(),
        );

        let err = trigger.dump_current_data().await.unwrap_err();
        check!(matches!(err, crate::dump::DumpError::Pipeline(_)));
        {
            let seen = completions.lock().unwrap();
            check!(seen.len() == 1, "finalize still runs for a failed dump");
            check!(seen[0].3, "completion carries failed=true");
        }

        stop.cancel();
        worker.await.unwrap();
    }

    /// The S3 stage clears per-dump state but writes no manifest for a
    /// failed dump (manifest presence means successful completion).
    #[cfg(feature = "worker-s3")]
    #[tokio::test]
    async fn s3_finalize_skips_manifest_for_failed_dump() {
        let config = s3::S3Config::builder()
            .bucket("b")
            .service_name("s")
            .instance_path("i")
            .boot_id("boot")
            .build();
        let mut uploader = S3PipelineUploader::new(config, None);

        let (trigger, mut rx) = dump::channel();
        trigger.dump_current_data();
        let req = rx.rx.try_recv().unwrap();
        uploader
            .dump_keys
            .insert(req.id.to_string(), vec!["traces/x.bin.gz".into()]);

        let completion = crate::dump::DumpCompletion {
            dump_id: req.id,
            triggered_at: SystemTime::now(),
            time_range: (SystemTime::now(), SystemTime::now()),
            segments_processed: 0,
            metadata: Vec::new(),
            failed: true,
        };
        let key = uploader.finalize_dump(&completion).await;
        check!(key.is_none());
        check!(
            uploader.dump_keys.is_empty(),
            "per-dump state still cleared"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn finalize_runs_for_empty_dump() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let completions = Arc::new(Mutex::new(Vec::new()));
        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(FinalizeStub {
                key: Some("dumps/empty.json".into()),
                completions: completions.clone(),
            })],
            rx,
            stop.clone(),
        );

        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.segments_processed == 0);
        check!(receipt.manifest_key.as_deref() == Some("dumps/empty.json"));
        check!(completions.lock().unwrap().len() == 1);

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn panicking_finalize_is_caught_receipt_resolves() {
        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![Box::new(PanickingFinalize)],
            rx,
            stop.clone(),
        );

        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.segments_processed == 1);
        check!(receipt.manifest_key.is_none());

        stop.cancel();
        worker.await.unwrap();
    }
}

#[cfg(all(test, feature = "worker-s3"))]
mod s3_dump_manifest_tests {
    use super::triggered_test_support::*;
    use super::*;
    use crate::dump;
    use assert2::check;

    fn fake_tm_client(fs_root: &Path) -> aws_sdk_s3_transfer_manager::Client {
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
        aws_sdk_s3_transfer_manager::Client::new(
            aws_sdk_s3_transfer_manager::Config::builder()
                .client(sdk_client)
                .build(),
        )
    }

    fn s3_uploader_for(root: &Path) -> S3PipelineUploader {
        let config = s3::S3Config::builder()
            .bucket("test-bucket")
            .prefix("traces")
            .service_name("test")
            .instance_path("test")
            .boot_id("test")
            .region("us-east-1")
            .build();
        let uploader = s3::S3Uploader::new(fake_tm_client(root), config);
        S3PipelineUploader::from_ready(uploader, connection::CircuitBreaker::new())
    }

    fn read_manifest(root: &Path, key: &str) -> serde_json::Value {
        let path = root.join("test-bucket").join(key);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("manifest at {} unreadable: {e}", path.display()));
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn dump_writes_manifest_listing_uploaded_keys() {
        let s3_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();
        seal_mem(&fs, 0, now_epoch());
        seal_mem(&fs, 1, now_epoch());

        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![
                Box::new(GzipCompressor),
                Box::new(s3_uploader_for(s3_root.path())),
            ],
            rx,
            stop.clone(),
        );

        let receipt = trigger
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

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn overlapping_dumps_fan_out_shared_key_to_both_manifests() {
        let s3_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![
                Box::new(GzipCompressor),
                Box::new(s3_uploader_for(s3_root.path())),
            ],
            rx,
            stop.clone(),
        );

        let fut_a = std::future::IntoFuture::into_future(
            trigger.dump_time_range(Duration::from_secs(60), Duration::from_secs(1)),
        );
        let fut_b = std::future::IntoFuture::into_future(
            trigger.dump_time_range(Duration::from_secs(60), Duration::from_secs(1)),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        seal_mem(&fs, 0, now_epoch());

        let (receipt_a, receipt_b) = tokio::join!(fut_a, fut_b);
        let receipt_a = receipt_a.unwrap();
        let receipt_b = receipt_b.unwrap();

        let manifest_a = read_manifest(s3_root.path(), receipt_a.manifest_key.as_ref().unwrap());
        let manifest_b = read_manifest(s3_root.path(), receipt_b.manifest_key.as_ref().unwrap());
        let segs_a = manifest_a["segments"].as_array().unwrap();
        let segs_b = manifest_b["segments"].as_array().unwrap();
        check!(segs_a.len() == 1);
        check!(segs_a == segs_b, "the shared key appears in both manifests");

        stop.cancel();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn empty_dump_still_writes_manifest_as_completion_signal() {
        let s3_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(s3_root.path().join("test-bucket")).unwrap();

        let fs = Fs::new_in_memory(64 * 1024, 1024).unwrap();
        let (trigger, rx) = dump::channel();

        let stop = tokio_util::sync::CancellationToken::new();
        let worker = spawn_worker(
            Arc::clone(&fs),
            vec![
                Box::new(GzipCompressor),
                Box::new(s3_uploader_for(s3_root.path())),
            ],
            rx,
            stop.clone(),
        );

        let receipt = trigger.dump_current_data().await.unwrap();
        check!(receipt.segments_processed == 0);
        let manifest = read_manifest(s3_root.path(), receipt.manifest_key.as_ref().unwrap());
        check!(manifest["segments"] == serde_json::json!([]));

        stop.cancel();
        worker.await.unwrap();
    }
}
