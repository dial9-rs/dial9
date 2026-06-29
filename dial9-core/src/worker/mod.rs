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

pub(crate) mod metrics;
pub(crate) mod pipeline_metrics;
/// Built-in segment processors (gzip, write-back).
pub mod processors;

use crate::dump::{DumpError, DumpReceipt, DumpRequest, Lookback};
use crate::fs::{EpochWindow, Fs, RemoveReason, TakenFiles, TakenSegment};
use crate::pipeline::{ProcessErrorKind, SegmentData, SegmentProcessor};
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    /// On-demand dump trigger receiver. When present the worker runs in
    /// triggered mode (see [`crate::dump`]): segments accumulate in the ring
    /// and the pipeline only runs on an explicit dump request. Wired by the
    /// facade builder; absent for continuous processing.
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

/// Spawn the segment-processing worker on a dedicated thread, draining
/// `writer`'s sealed segments through `config`'s processor pipeline. Returns the
/// thread handle, or `None` when `writer` has no filesystem backend.
/// `thread_init` runs on the worker thread before the loop and returns
/// a teardown closure run after it (e.g. profiler register/unregister).
///
/// Owns the fs handoff so callers never touch the writer's storage backend.
pub fn spawn<M, Init, Teardown>(
    writer: &crate::writer::SegmentWriter<M>,
    config: BackgroundTaskConfig,
    shutdown: tokio::sync::oneshot::Receiver<Duration>,
    thread_init: Init,
) -> Option<crate::primitives::thread::JoinHandle<()>>
where
    M: crate::writer::WriterMode,
    Init: FnOnce() -> Teardown + Send + 'static,
    Teardown: FnOnce(),
{
    let fs = writer.fs_handle()?;
    Some(crate::primitives::thread::spawn_named(
        "dial9-worker",
        move || {
            let teardown = thread_init();
            run_background_task(config, shutdown, fs);
            teardown();
        },
    ))
}

/// Consumer side of the bus: drains sealed segments and runs each through the
/// configured [`SegmentProcessor`] pipeline. Built and driven by
/// [`run_background_task`].
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

    /// Total failure: a captured segment failed terminally and nothing made
    /// it through. Drives both the `Err` receipt and the S3 stage skipping
    /// the manifest.
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

/// Record a terminal pipeline error against every matched dump that has none
/// yet. The first dump takes `kind` itself; the rest get an `Io`-wrapped
/// copy of its message (`ProcessErrorKind` is not `Clone`).
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

            Self::wait_for_more(&self.fs, &self.stop, self.poll_interval).await;
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
                // Resolve every dump whose forward deadline elapsed (or that
                // never had one) and that is not held open by a pending
                // retry. A disk pass covers the whole backlog, so a retry
                // there only holds the dumps the retrying segment matched;
                // the memory pop dispenses one slot per pass, so a retry
                // there keeps everything open until the head of the ring
                // settles (budget-bounded, brief).
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
                // One final matching pass picks up segments sealed by writer
                // finalization, then every open dump resolves with a
                // truncated receipt covering what actually landed.
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
                _ = Self::wait_for_more(&self.fs, &self.stop, self.poll_interval),
                    if !dumps.is_empty() => {}
            }
        }
    }

    /// Run matching passes until the active windows quiesce. Returns the ids
    /// of dumps matched by segments that failed retryably (the caller bails
    /// to its select instead of hot-looping the retry and keeps those dumps
    /// open); empty means the windows quiesced.
    async fn drain_matching(&mut self, dumps: &mut [ActiveDump]) -> Vec<crate::dump::DumpId> {
        loop {
            if dumps.is_empty() {
                return Vec::new();
            }
            let windows: Vec<EpochWindow> = dumps.iter().map(|d| d.window).collect();
            let mut taken = self.fs.take_files_matching(&windows);
            // Prune cache entries for files no longer dispensed (disk
            // dispenses every unclaimed file per pass, so absence means the
            // writer evicted it).
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
            // Out-of-window claims are released, not dispatched; only count
            // segments that actually entered the pipeline.
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
            // Same panic discipline as `process()`: a panicking finalize is
            // caught, logged, and the receipt still resolves.
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
    #[cfg(test)]
    async fn process_open_segments(&mut self) -> bool {
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
                        // Defensive: the windowed pop only dispenses matching
                        // slots. Put the bytes back without burning a retry
                        // attempt.
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

            if !matched.is_empty() {
                // Every matched dump's id rides the segment, comma-joined;
                // caller correlation pairs are namespaced `dump.{key}` and
                // the first-registered dump wins on conflicts.
                let ids: Vec<String> = matched.iter().map(|&i| dumps[i].id.to_string()).collect();
                data.metadata_mut().insert("dump_id".into(), ids.join(","));
                for &i in &matched {
                    for (k, v) in &dumps[i].metadata {
                        data.metadata_mut()
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
                        data.adjust_accounting();
                        stage.succeed();
                        metrics.pipeline.push(processor.name(), stage);
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(target: "dial9_worker", processor = processor.name(), segment = seg_idx + 1, elapsed_ms = proc_start.elapsed().as_secs_f64() * 1000.0, error = %e.kind(), "processor failed");
                        let (next_data, err_kind) = e.into_parts();
                        data = next_data;
                        let already_deleted = err_kind.already_deleted();
                        let retryable = err_kind.retryable();
                        let kind_msg = err_kind.to_string();
                        stage.fail();
                        metrics.pipeline.push(processor.name(), stage);
                        metrics.status = Some(MetriqueResult::Failure);
                        metrics.compressed_size = data.compressed_size();
                        metrics.total_time.stop();
                        if already_deleted {
                            tracing::debug!(target: "dial9_worker", id = %data.segment(), "segment evicted during processing, skipping");
                            // Best-effort: an evicted segment leaves the dump
                            // silently uncounted.
                            self.epoch_cache.remove(&seg_ref_retained.index());
                            if !matched.is_empty() {
                                stats.matched_done += 1;
                            }
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
                                                // Budget spent: terminal for any
                                                // matched dump, same as a
                                                // non-retryable failure.
                                                if !matched.is_empty() {
                                                    stats.matched_done += 1;
                                                    record_dump_error(dumps, &matched, err_kind);
                                                }
                                            } else {
                                                tokio::time::sleep(self.poll_interval).await;
                                                self.fs.release_for_retry(
                                                    data.segment(),
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
                                                tracing::warn!(target: "dial9_worker", id = %data.segment(), "memory segment missing retry state, dropping");
                                            });
                                            if !matched.is_empty() {
                                                stats.matched_done += 1;
                                                record_dump_error(dumps, &matched, err_kind);
                                            }
                                        }
                                    }
                                }
                                SegmentRef::Disk(_) => {
                                    tracing::debug!(target: "dial9_worker", id = %data.segment(), err = %kind_msg, "retryable error");
                                    self.fs.release_claim(data.segment());
                                    stats
                                        .retry_dump_ids
                                        .extend(matched.iter().map(|&i| dumps[i].id));
                                }
                            }
                        } else {
                            self.fs
                                .remove_sealed(data.segment(), RemoveReason::Terminal);
                            rate_limited!(Duration::from_secs(60), {
                                tracing::warn!(target: "dial9_worker", error = %kind_msg, id = %data.segment(), "processor failed, removing segment");
                            });
                            self.epoch_cache.remove(&seg_ref_retained.index());
                            if !matched.is_empty() {
                                stats.matched_done += 1;
                                record_dump_error(dumps, &matched, err_kind);
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
                        // `data` (and the future) were consumed by the panic.
                        // The metrics guard is a separate local, so record the
                        // panic on it directly. It flushes on drop below.
                        metrics.status = Some(MetriqueResult::Failure);
                        metrics.panicked = true;
                        metrics.panic_message = Some(panic_msg.to_owned());
                        metrics.total_time.stop();
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

            metrics.status = Some(MetriqueResult::Success);
            metrics.compressed_size = data.compressed_size();
            metrics.total_time.stop();
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

#[cfg(test)]
mod tests;
