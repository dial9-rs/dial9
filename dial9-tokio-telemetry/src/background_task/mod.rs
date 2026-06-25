pub(crate) mod boot_id;
#[cfg(test)]
pub(crate) mod testutil;

pub use dial9_core::pipeline::{MemorySegment, Payload, SealedSegment, SegmentRef};

use crate::telemetry::writer::{Disk, WriterMode};
use std::marker::PhantomData;
use std::path::PathBuf;
#[cfg(feature = "cpu-profiling")]
use {crate::rate_limit::rate_limited, std::future::Future, std::pin::Pin, std::time::Duration};

pub use dial9_core::pipeline::{ProcessError, ProcessErrorKind, SegmentData, SegmentProcessor};
pub use dial9_core::worker::BackgroundTaskConfig;
pub(crate) use dial9_core::worker::processors::{GzipCompressor, WriteBackProcessor};
#[cfg(feature = "worker-s3")]
pub use dial9_core::worker::s3;
#[cfg(feature = "worker-s3")]
pub(crate) use dial9_core::worker::s3::S3PipelineUploader;
pub(crate) use dial9_core::worker::{DEFAULT_POLL_INTERVAL, run_background_task};

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
        self.processors
            .push(Box::new(WriteBackProcessor::to_dir(dir.into())));
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
            if data.payload().starts_with(&[0x1f, 0x8b]) {
                tracing::debug!(target: "dial9_worker", "segment is gzip-compressed, skipping symbolization");
                return Ok(data);
            }
            // The symbolize FFI reads `&[u8]`, so we materialize a single
            // contiguous `Bytes`. When there's only one chunk this is a
            // zero-copy `Bytes::clone`-equivalent; the `BytesMut` concat
            // path runs only on already-segmented input (rare).
            let input = data.take_payload().into_bytes();
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
                    data.set_payload(payload);
                    Ok(data)
                }
                Ok(Err(e)) => {
                    rate_limited!(Duration::from_secs(60), {
                        tracing::warn!(target: "dial9_worker", error = %e, "symbolization failed, preserving original bytes");
                    });
                    Err(ProcessError::io(data, e))
                }
                Err(e) => Err(ProcessError::io(data, std::io::Error::other(e))),
            }
        })
    }
}

#[cfg(test)]
mod e2e_writer_tests {
    use assert2::check;
    use dial9_core::worker::WorkerLoop;
    use std::time::Duration;

    // --- End-to-end memory writer → worker pipeline ---
    // These drive the real `InMemoryWriter` (which lives in dial9-core) through
    // the worker loop, so they stay here on the telemetry side where the worker
    // and the `Dial9Event` decoder live.

    /// Encode a single park event into a self-contained batch (header + event),
    /// matching the format produced by ThreadLocalBuffer.
    fn mem_test_batch() -> crate::telemetry::Batch {
        use crate::telemetry::format::{WorkerId, WorkerParkEvent};
        use dial9_trace_format::encoder::Encoder;
        let mut enc = Encoder::new_to(Vec::new()).unwrap();
        enc.write_infallible(&WorkerParkEvent {
            timestamp_ns: 1000,
            worker_id: WorkerId::from(0usize),
            local_queue: 2,
            cpu_time_ns: 0,
            tid: 0,
        });
        crate::telemetry::Batch::new(enc.into_inner(), 1)
    }

    /// Drive a memory writer through the real worker pipeline and return the
    /// captured per-segment payloads. Exercises the full mechanism: write ->
    /// Fs::Mem seal -> ring -> finalize (mark_writer_done) -> WorkerLoop::run
    /// drain-to-empty -> processor.
    async fn run_mem_e2e(
        mut writer: crate::telemetry::writer::InMemoryWriter,
        events: usize,
    ) -> Vec<Vec<u8>> {
        let fs = writer.fs_handle().expect("memory writer exposes its Fs");
        for _ in 0..events {
            writer.write_encoded_batch(&mem_test_batch()).unwrap();
        }
        // Seals the active segment onto the ring and signals writer_done.
        writer.finalize().unwrap();

        let (capture, captured) = super::testutil::CapturingProcessor::new();
        // stop is never cancelled: the loop exits via writer_done only.
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            fs,
            Duration::from_millis(5),
            vec![Box::new(capture)],
            stop,
            metrique_writer::sink::DevNullSink::boxed(),
        );
        worker.run().await;

        let segments = captured.lock().unwrap();
        segments.clone()
    }

    /// Count decoded payload events across `segments`, dropping the per-segment
    /// metadata/clock-sync framing the writer emits.
    fn count_payload_events(segments: &[Vec<u8>]) -> usize {
        use crate::telemetry::analysis_events::Dial9Event;

        super::testutil::decode_captured(segments)
            .into_iter()
            .filter(|e| {
                !matches!(
                    e,
                    Dial9Event::SegmentMetadataEvent(..) | Dial9Event::ClockSyncEvent(..)
                )
            })
            .count()
    }

    #[tokio::test]
    async fn mem_writer_e2e_delivers_all_events() {
        const EVENTS: usize = 25;

        let segments = run_mem_e2e(
            crate::telemetry::writer::InMemoryWriter::new(1 << 20).unwrap(),
            EVENTS,
        )
        .await;

        check!(!segments.is_empty(), "worker captured no segments");
        check!(
            count_payload_events(&segments) == EVENTS,
            "every written event must reach the processor"
        );
    }

    /// Same as `mem_writer_e2e_delivers_all_events`, but a tiny `max_segment_size`
    /// forces several rotations so the worker delivers multiple sealed segments.
    #[tokio::test]
    async fn mem_writer_e2e_delivers_all_events_across_rotations() {
        const EVENTS: usize = 60;

        // Huge ring (nothing evicts) + tiny segments (rotate every few batches).
        let writer = crate::telemetry::writer::InMemoryWriter::builder()
            .max_total_size(16 * 1024 * 1024)
            .max_segment_size(256)
            .build()
            .unwrap();
        let segments = run_mem_e2e(writer, EVENTS).await;

        check!(
            segments.len() >= 2,
            "tiny segments must force rotation, got {} segment(s)",
            segments.len()
        );
        check!(
            count_payload_events(&segments) == EVENTS,
            "every event across all rotated segments must reach the processor"
        );
    }
}
