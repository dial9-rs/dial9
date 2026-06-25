//! The segment-processing worker.
//!
//! [`WorkerLoop`] is the consumer side of the bus: it drains sealed segments
//! and runs each through a [`SegmentProcessor`](crate::pipeline::SegmentProcessor)
//! pipeline (compress, symbolize, upload, write-back). It is the generic
//! executor of the pipeline trait; the processors themselves are supplied by
//! the caller.
//!
//! Behind the `pipeline` feature: the worker builds a tokio runtime to drive
//! the async processors. Core's default build stays runtime-agnostic without it.

/// S3 upload circuit breaker.
#[cfg(feature = "pipeline-s3")]
pub(crate) mod connection;
#[cfg(feature = "pipeline-s3")]
pub(crate) mod instance_metadata;
pub(crate) mod metrics;
pub(crate) mod pipeline_metrics;
/// Built-in segment processors (gzip, write-back).
pub mod processors;
/// Built-in S3 upload processor.
#[cfg(feature = "pipeline-s3")]
pub mod s3;

use crate::fs::{Fs, RemoveReason, TakenFiles, TakenSegment};
use crate::pipeline::{SegmentData, SegmentProcessor};
use crate::rate_limit::rate_limited;
use crate::sealed::{self, SegmentRef};
use crate::worker::metrics::{Operation, SegmentProcessMetrics, WorkerCycleMetrics};
use crate::worker::pipeline_metrics::{MetriqueResult, PipelineMetrics, StageMetrics};
use futures_util::FutureExt;
use metrique::timers::Timer;
use metrique::writer::BoxEntrySink;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

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
    /// Metrics sink. Defaults to [`DevNullSink`](metrique::writer::sink::DevNullSink).
    #[builder(default = metrique::writer::sink::DevNullSink::boxed())]
    metrics_sink: BoxEntrySink,
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
    pub fn trace_path(&self) -> Option<&Path> {
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

/// The worker loop function. Runs on a dedicated thread, polls for sealed
/// segments and processes them through the configured pipeline.
///
/// Creates a single-threaded tokio runtime for async processors (e.g. S3 upload).
/// The worker is a "good citizen": it will lose data rather than disrupt the application.
pub fn run_background_task(
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

    tracing::info!(target: "dial9_worker", dir = %config.trace_dir().display(), stem = %config.trace_stem(), processors = processors.len(), "worker started");
    rt.block_on(async {
        let stop = tokio_util::sync::CancellationToken::new();
        let mut worker = WorkerLoop::new(
            fs,
            config.poll_interval(),
            processors,
            stop.clone(),
            metrics_sink,
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

crate::test_util_pub! {
/// Consumer side of the bus: drains sealed segments and runs each through the
/// configured [`SegmentProcessor`] pipeline. Built and driven by
/// [`run_background_task`]; `pub` only under `test-util` for cross-crate tests.
struct WorkerLoop {
    fs: Arc<Fs>,
    poll_interval: Duration,
    processors: Vec<Box<dyn SegmentProcessor>>,
    metrics_sink: BoxEntrySink,
    /// When cancelled, the worker finishes its current cycle and exits
    /// instead of sleeping.
    stop: tokio_util::sync::CancellationToken,
}
}

impl WorkerLoop {
    pub fn new(
        fs: Arc<Fs>,
        poll_interval: Duration,
        processors: Vec<Box<dyn SegmentProcessor>>,
        stop: tokio_util::sync::CancellationToken,
        metrics_sink: BoxEntrySink,
    ) -> Self {
        Self {
            fs,
            poll_interval,
            processors,
            metrics_sink,
            stop,
        }
    }

    pub async fn run(&mut self) {
        loop {
            let taken = self.fs.take_files();
            let dispatched = taken.segments.len() as u64;
            self.emit_cycle_metrics(&taken, dispatched);
            self.process_segments(taken.segments).await;

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
                    self.process_segments(taken.segments).await;
                }
            }

            Self::wait_for_more(&self.fs, &self.stop, self.poll_interval).await;
        }
    }

    /// Park until new segments may be available or we're told to stop.
    ///
    /// Disk polls on `poll_interval`, memory awaits the ring's wakeup via [`Fs::wait_for_wakeup`].
    ///
    /// Borrows only `fs` and `stop` (both `Sync`) rather than `&self`, so the
    /// `run()` future stays `Send` (`WorkerLoop` holds non-`Sync` processors).
    async fn wait_for_more(
        fs: &Fs,
        stop: &tokio_util::sync::CancellationToken,
        poll_interval: Duration,
    ) {
        if fs.is_disk() {
            tokio::select! {
                _ = stop.cancelled() => {}
                _ = tokio::time::sleep(poll_interval) => {}
            }
        } else {
            tokio::select! {
                _ = stop.cancelled() => {}
                _ = fs.wait_for_wakeup() => {}
            }
        }
    }

    // Prod drains via the `run()` shutdown loop (stop/writer_done ->
    // drain-to-empty). This forces one synchronous drain cycle for unit tests.
    #[cfg(all(test, feature = "pipeline-s3"))]
    async fn process_open_segments(&mut self) -> bool {
        let taken = self.fs.take_files();
        let found = !taken.segments.is_empty();
        let dispatched = taken.segments.len() as u64;
        self.emit_cycle_metrics(&taken, dispatched);
        self.process_segments(taken.segments).await;
        found
    }

    async fn process_segments(&mut self, segments: Vec<TakenSegment>) {
        if self.processors.is_empty() {
            return;
        }

        'next_segment: for (seg_idx, taken) in segments.into_iter().enumerate() {
            // Snapshot memory-only retry state before `load()` consumes
            // `taken`, so re-dispense on a retryable failure gets the same bytes as the first attempt.
            let retry_count = taken.retry_count();
            let original_bytes = taken.original_bytes();
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

            let mut metrics = SegmentProcessMetrics {
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
            let mut data = SegmentData::new(
                seg_ref,
                payload,
                HashMap::from([
                    ("epoch_secs".into(), epoch_secs.to_string()),
                    ("segment_index".into(), seg_ref_retained.index().to_string()),
                ]),
                accounting,
            );

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
                        data.adjust_accounting();
                        stage.succeed();
                        metrics.pipeline.push(processor.name(), stage);
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(target: "dial9_worker", processor = processor.name(), segment = seg_idx + 1, elapsed_ms = proc_start.elapsed().as_secs_f64() * 1000.0, error = %e.kind(), "processor failed");
                        let already_deleted = e.kind().already_deleted();
                        let retryable = e.kind().retryable();
                        let kind_msg = e.kind().to_string();
                        data = e.into_data();
                        stage.fail();
                        metrics.pipeline.push(processor.name(), stage);
                        metrics.status = Some(MetriqueResult::Failure);
                        metrics.compressed_size = data.compressed_size();
                        metrics.total_time.stop();
                        if already_deleted {
                            tracing::debug!(target: "dial9_worker", id = %data.segment(), "segment evicted during processing, skipping");
                        } else if retryable {
                            match data.segment() {
                                // Memory segments always carry retry_count + a
                                // byte snapshot (set in `TakenSegment::memory`).
                                // If either is missing the invariant broke. In-flight
                                // is released via `data`'s accounting on `continue`.
                                SegmentRef::Memory(_) => {
                                    match (retry_count, original_bytes.as_ref()) {
                                        (Some(prev), Some(bytes)) => {
                                            let attempt = prev + 1;
                                            if attempt > crate::fs::MEMORY_RETRY_BUDGET {
                                                rate_limited!(Duration::from_secs(60), {
                                                    tracing::warn!(target: "dial9_worker", id = %data.segment(), err = %kind_msg, budget = crate::fs::MEMORY_RETRY_BUDGET, "memory retry budget exhausted, dropping segment");
                                                });
                                            } else {
                                                tokio::time::sleep(self.poll_interval).await;
                                                self.fs.release_for_retry(
                                                    data.segment(),
                                                    bytes.clone(),
                                                    attempt,
                                                );
                                            }
                                        }
                                        _ => {
                                            rate_limited!(Duration::from_secs(60), {
                                                tracing::warn!(target: "dial9_worker", id = %data.segment(), "memory segment missing retry state, dropping");
                                            });
                                        }
                                    }
                                }
                                SegmentRef::Disk(_) => {
                                    tracing::debug!(target: "dial9_worker", id = %data.segment(), err = %kind_msg, "retryable error");
                                    self.fs.release_claim(data.segment());
                                }
                            }
                        } else {
                            self.fs
                                .remove_sealed(data.segment(), RemoveReason::Terminal);
                            rate_limited!(Duration::from_secs(60), {
                                tracing::warn!(target: "dial9_worker", error = %kind_msg, id = %data.segment(), "processor failed, removing segment");
                            });
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
                        // `data` (and the future) were consumed by the panic.
                        // The metrics guard is a separate local, so record the
                        // panic on it directly. It flushes on drop below.
                        metrics.status = Some(MetriqueResult::Failure);
                        metrics.panicked = true;
                        metrics.panic_message = Some(panic_msg.to_owned());
                        metrics.total_time.stop();
                        self.fs
                            .remove_sealed(&seg_ref_retained, RemoveReason::Terminal);
                        continue 'next_segment;
                    }
                }
            }

            metrics.status = Some(MetriqueResult::Success);
            metrics.compressed_size = data.compressed_size();
            metrics.total_time.stop();
        }
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

#[cfg(test)]
mod tests;
