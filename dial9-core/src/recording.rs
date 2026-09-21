use crate::buffer::{BufferMode, SegmentWriter};
use crate::flush_loop::run_flush_loop;
use crate::handle::{ControlCommand, Dial9Handle, InstallGlobalHandleError};
use crate::primitives::sync::{Arc, Mutex};
use crate::primitives::{sync::mpsc, thread::JoinHandle};
use crate::recorder::SoleRecorderGuard;
use crate::shared_state::SharedState;
use std::time::Duration;

/// The background worker thread and its stop signal.
///
/// Present only when a segment-processing pipeline is configured.
#[cfg(feature = "pipeline")]
pub(crate) struct WorkerHandle {
    shutdown: Option<tokio::sync::oneshot::Sender<Duration>>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(feature = "pipeline")]
impl WorkerHandle {
    /// Wrap the worker's shutdown sender and join handle.
    pub(crate) fn new(
        shutdown: tokio::sync::oneshot::Sender<Duration>,
        thread: JoinHandle<()>,
    ) -> Self {
        Self {
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }
}

/// Owns the recording state: the [`Dial9Handle`], the flush thread, and (with
/// the `pipeline` feature) the background worker.
///
/// This is an RAII guard: dropping it flushes remaining events, seals the final
/// segment, and stops the worker. For a bounded drain of the background worker
/// (symbolize, compress, upload) call [`graceful_shutdown`](Self::graceful_shutdown)
/// instead.
pub struct Recorder {
    handle: Dial9Handle,
    flush_thread: Option<JoinHandle<()>>,
    /// Hooks run once, with the handle, on the first `enable()`.
    recording_start_hooks: Mutex<Vec<RecordingStartHook>>,
    /// Held while this is the process's recorder. Dropping it frees the slot.
    sole_recorder: Option<SoleRecorderGuard>,
    #[cfg(feature = "pipeline")]
    worker: Option<WorkerHandle>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("enabled", &self.handle.is_enabled())
            .field("recording", &self.flush_thread.is_some())
            .finish_non_exhaustive()
    }
}

/// A hook run once, with the live [`Dial9Handle`], when the recorder first
/// enables recording.
pub type RecordingStartHook = Box<dyn FnOnce(&Dial9Handle) + Send>;

impl Recorder {
    /// Create a recorder from an existing handle and flush thread.
    pub(crate) fn new(handle: Dial9Handle, flush_thread: Option<JoinHandle<()>>) -> Self {
        Self {
            handle,
            flush_thread,
            recording_start_hooks: Mutex::new(Vec::new()),
            sole_recorder: None,
            #[cfg(feature = "pipeline")]
            worker: None,
        }
    }

    /// Hold the process's recorder slot for this recorder's lifetime.
    pub(crate) fn hold_process(&mut self, guard: crate::recorder::SoleRecorderGuard) {
        self.sole_recorder = Some(guard);
    }

    /// Install the one-shot hooks to run on the first `enable()`.
    pub(crate) fn set_recording_start_hooks(&self, hooks: Vec<RecordingStartHook>) {
        *self.recording_start_hooks.lock().unwrap() = hooks;
    }

    /// Start recording over `shared`: build the recording [`Dial9Handle`], spawn
    /// the flush thread that drains the bus into `writer`, and own its lifecycle.
    ///
    /// The flush-thread control channel is created and owned internally; reach
    /// the handle via [`handle`](Self::handle).
    ///
    /// `thread_init` runs once on the flush thread before the loop and returns
    /// a teardown closure run after it — use it to register/unregister the
    /// thread with a runtime's profiler.
    ///
    /// Pass `None` for `flush_metrics_sink` to discard flush metrics.
    pub(crate) fn start<M, Init, Teardown>(
        shared: Arc<SharedState>,
        writer: SegmentWriter<M>,
        flush_metrics_sink: Option<metrique::writer::BoxEntrySink>,
        thread_init: Init,
    ) -> Self
    where
        M: BufferMode + Send + 'static,
        Init: FnOnce() -> Teardown + Send + 'static,
        Teardown: FnOnce(),
    {
        let (control_tx, control_rx) = mpsc::sync_channel(1);
        let handle = Dial9Handle::enabled(shared.clone(), control_tx);
        let flush_metrics_sink =
            flush_metrics_sink.unwrap_or_else(metrique::writer::sink::DevNullSink::boxed);
        let flush_thread = crate::primitives::thread::spawn_named("dial9-flush", move || {
            // The flush thread is latency-tolerant; lower its priority.
            #[cfg(target_os = "linux")]
            // SAFETY: nice() is a simple syscall with no memory-safety
            // implications; lowering priority is always permitted unprivileged.
            unsafe {
                let _ = libc::nice(10);
            }
            let teardown = thread_init();
            run_flush_loop(control_rx, &shared, &flush_metrics_sink, writer);
            teardown();
        });
        Self::new(handle, Some(flush_thread))
    }

    /// The recording handle for this recorder.
    pub fn handle(&self) -> &Dial9Handle {
        &self.handle
    }

    /// Publish this recorder's handle as the process-global one.
    ///
    /// When set, [`Dial9Handle::current`] resolves on every thread in the
    /// process. When not set, it resolves only on threads a runtime integration
    /// has installed a handle on.
    ///
    /// Returns [`InstallGlobalHandleError`] and changes nothing if another handle
    /// is already installed: two live globals would split one process's events
    /// across two traces. A recorder clears its own when it stops, so a later
    /// install succeeds.
    ///
    /// ```no_run
    /// use dial9_core::buffer::MemoryBuffer;
    /// use dial9_core::handle::Dial9Handle;
    /// use dial9_core::recorder::recorder;
    /// use dial9_trace_format::TraceEvent;
    ///
    /// #[derive(TraceEvent)]
    /// struct Tick {
    ///     #[traceevent(timestamp)]
    ///     timestamp_ns: u64,
    /// }
    ///
    /// let rec = recorder(MemoryBuffer::new(1 << 20)?).build();
    /// rec.install_global_handle()?;
    ///
    /// std::thread::spawn(|| {
    ///     // reachable here, with no handle plumbed in
    ///     Dial9Handle::current().record_event(Tick { timestamp_ns: 0 });
    /// });
    /// # Ok::<_, Box<dyn std::error::Error>>(())
    /// ```
    pub fn install_global_handle(&self) -> Result<(), InstallGlobalHandleError> {
        crate::handle::set_global_handle(self.handle.clone())
    }

    /// Attach the background worker to this recorder, so its lifecycle is tied
    /// to the recorder's (drained on `graceful_shutdown`, stopped on drop).
    #[cfg(feature = "pipeline")]
    pub(crate) fn attach_worker(&mut self, worker: WorkerHandle) {
        self.worker = Some(worker);
    }

    crate::test_util_pub! {
        /// The shared recording state.
        fn shared(&self) -> Option<&Arc<SharedState>> {
            self.handle.shared()
        }
    }

    /// Monotonic start time of the recorder in nanoseconds.
    pub fn start_time(&self) -> Option<u64> {
        self.shared().map(|s| s.start_time_ns())
    }

    /// Enable recording.
    pub fn enable(&self) {
        self.handle.enable();
        // Run the one-shot start hooks now that the handle is live and
        // recording. Draining leaves them run-once across repeated enables.
        let hooks = std::mem::take(&mut *self.recording_start_hooks.lock().unwrap());
        for hook in hooks {
            hook(&self.handle);
        }
    }

    /// Disable recording.
    pub fn disable(&self) {
        self.handle.disable();
    }

    /// Flush remaining events, seal the final segment, and join the flush thread.
    ///
    /// Call this before dropping any runtime state that owns worker threads, so
    /// that their thread-local buffers have already been flushed to the central
    /// collector.
    pub(crate) fn stop_flush_thread(&mut self) {
        // Clear the global before the blocking flush below, otherwise other threads
        // keep resolving it and recording into buffers that nothing will drain.
        if let Some(shared) = self.handle.shared() {
            crate::handle::clear_global_handle_for(shared);
        }

        // Drain the calling thread's local buffer — it won't get a thread-stop
        // hook, so any unflushed events would be lost otherwise.
        if let Some(shared) = self.handle.shared() {
            crate::encoder::drain_to_collector(&shared.collector);
        }

        // Tell the flush thread to do a final flush + finalize, then exit.
        let (ack_tx, ack_rx) = mpsc::sync_channel(0);
        if let Some(tx) = self.handle.control_tx()
            && tx.send(ControlCommand::FinalizeAndStop(ack_tx)).is_ok()
        {
            let _ = ack_rx.recv();
        }
        if let Some(t) = self.flush_thread.take() {
            let _ = t.join();
        }

        // Stop is permanent from here: recording off, enable() and new
        // attaches refused. Nothing drains sources once the flush thread is
        // gone, so release them and whatever they own.
        if let Some(shared) = self.handle.shared() {
            shared.mark_stopped();
            shared.clear_sources();
        }

        // Runtime threads drop their handle in a thread-stop hook, but the
        // thread that attached the runtime gets no such hook and would hold a
        // handle to a stopped recorder for the rest of its life.
        crate::handle::clear_tl_handle();
    }

    /// Flush remaining events, seal the final segment, and (with `pipeline`)
    /// wait for the background worker to drain within `timeout`.
    ///
    /// Call this after any runtime that owns worker threads has been dropped, so
    /// their thread-local buffers have already been flushed. Consumes the
    /// recorder so `Drop` becomes a no-op.
    ///
    /// Failures during draining are logged.
    pub fn graceful_shutdown(mut self, timeout: Duration) {
        // `timeout` only bounds the worker drain, which exists under `pipeline`.
        #[cfg(not(feature = "pipeline"))]
        let _ = timeout;

        // 1. Flush + finalize the last segment.
        self.stop_flush_thread();

        // 2. Signal the worker to drain, then join it.
        #[cfg(feature = "pipeline")]
        if let Some(w) = &mut self.worker {
            if let Some(tx) = w.shutdown.take() {
                let _ = tx.send(timeout);
            }
            if let Some(t) = w.thread.take()
                && let Err(e) = t.join()
            {
                tracing::error!(target: "dial9", panic = ?e, "worker thread panicked during shutdown");
            }
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // 1. Flush + finalize. Idempotent, so a prior graceful_shutdown/stop is fine.
        self.stop_flush_thread();

        // 2. Hard shutdown: drop the sender without sending — the worker sees a
        // closed channel and exits without draining. For a graceful drain, call
        // graceful_shutdown() instead.
        #[cfg(feature = "pipeline")]
        if let Some(w) = &mut self.worker {
            w.shutdown.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{DiskBuffer, MemoryBuffer};
    use crate::recorder::recorder;
    use crate::source::{FlushContext, Source};
    use crate::test_support::{decode_segment_metadata, sealed_segment};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    // ── Test fixtures ────────────────────────────────────────────────

    struct PanickingFlushSource(&'static str);
    impl Source for PanickingFlushSource {
        fn flush(&mut self, _ctx: &FlushContext<'_>) {
            panic!("PanickingFlushSource({}) intentionally panics", self.0);
        }
        fn name(&self) -> &'static str {
            self.0
        }
    }

    struct PanickingMetadataSource(&'static str);
    impl Source for PanickingMetadataSource {
        fn flush(&mut self, _ctx: &FlushContext<'_>) {}
        fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
            out.push((
                format!("{}.partial", self.0),
                "should not survive".to_string(),
            ));
            panic!("PanickingMetadataSource({}) intentionally panics", self.0);
        }
        fn name(&self) -> &'static str {
            self.0
        }
    }

    struct HealthyMetadataSource;
    impl Source for HealthyMetadataSource {
        fn flush(&mut self, _ctx: &FlushContext<'_>) {}
        fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
            out.push(("healthy.key".to_string(), "healthy-value".to_string()));
        }
        fn name(&self) -> &'static str {
            "healthy_metadata"
        }
    }

    #[derive(Debug, dial9_trace_format::TraceEvent)]
    struct MarkerEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
    }

    /// Records the `source` field of every WARN event seen while active.
    struct SourceWarnSubscriber {
        warned_sources: Arc<Mutex<Vec<String>>>,
    }

    struct SourceFieldVisitor(Option<String>);
    impl tracing::field::Visit for SourceFieldVisitor {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "source" {
                self.0 = Some(value.to_string());
            }
        }
        fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
    }

    impl tracing::Subscriber for SourceWarnSubscriber {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = SourceFieldVisitor(None);
            event.record(&mut visitor);
            if let Some(name) = visitor.0 {
                self.warned_sources.lock().unwrap().push(name);
            }
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    // ── Tests ────────────────────────────────────────────────────────

    /// `teardown()` should run even after an uncaught `Source::flush` panic:
    /// it's the flush thread's own cleanup, unrelated to whichever source
    /// misbehaved. `flush_sources` now catches and drops a panicking
    /// source's cycle, so `run_flush_loop` returns normally and `teardown()`
    /// runs as it would for any clean stop.
    #[test]
    fn source_panic_does_not_skip_thread_teardown() {
        let teardown_ran = Arc::new(AtomicBool::new(false));
        let teardown_ran_for_thread = teardown_ran.clone();

        let writer = MemoryBuffer::builder()
            .max_total_size(1024 * 1024)
            .max_segment_size(256)
            .build()
            .unwrap();

        let mut recorder = recorder(writer)
            .source(PanickingFlushSource("panicking"))
            .on_recording_thread_start(move || {
                let teardown_ran_for_thread = teardown_ran_for_thread.clone();
                move || {
                    teardown_ran_for_thread.store(true, Ordering::Relaxed);
                }
            })
            .build();
        recorder.handle().enable();

        // Give the flush thread time to run at least one cycle: the
        // panicking source guarantees the very first cycle panics.
        std::thread::sleep(Duration::from_millis(200));

        // Explicitly ask the flush thread to stop, rather than relying on
        // the sleep alone: this is what makes the loop return normally and
        // run teardown().
        recorder.stop_flush_thread();

        assert!(
            teardown_ran.load(Ordering::Relaxed),
            "teardown() should still run even after an uncaught Source panic during flush, \
             but it was skipped"
        );
    }

    /// A panicking `Source::segment_metadata` must not corrupt sibling
    /// sources' entries for the same cycle, and its own partial push must
    /// not survive. `flush_loop` now catches the panic and truncates
    /// `source_entries` back to its pre-call length.
    #[test]
    fn source_panic_during_segment_metadata_skips_only_that_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).expect("writer");

        let recorder = recorder(writer)
            .source(PanickingMetadataSource("panicking_metadata"))
            .source(HealthyMetadataSource)
            .build();
        recorder.handle().enable();
        // A trivial marker event: `finalize()` discards a segment that never
        // held a real event, so without this the metadata-only segment
        // below would never get sealed at all.
        recorder
            .handle()
            .record_event(MarkerEvent { timestamp_ns: 0 });
        recorder.graceful_shutdown(Duration::ZERO);

        let bytes = std::fs::read(sealed_segment(dir.path())).expect("read segment");
        let entries = decode_segment_metadata(&bytes);

        assert_eq!(
            entries.get("healthy.key").map(String::as_str),
            Some("healthy-value"),
            "sibling source's metadata must survive a panicking source in the same cycle"
        );
        assert!(
            !entries.contains_key("panicking_metadata.partial"),
            "a panicking source's partial push must not survive in the cycle's metadata"
        );
        assert_eq!(
            entries
                .get("dial9.source.panicking_metadata.panicked")
                .map(String::as_str),
            Some("true"),
            "the trace itself should record which source panicked, not just a log line"
        );
    }

    /// Two distinct sources panicking during `flush` in the same cycle must
    /// each be warned about: `catch_source_panic`'s rate limiting is keyed
    /// per source name, so one source's warning can't suppress the other's
    /// through their shared `flush_sources` call site.
    #[test]
    fn distinct_panicking_flush_sources_are_each_warned_about() {
        let warned_sources = Arc::new(Mutex::new(Vec::new()));
        let subscriber_source = warned_sources.clone();

        let writer = MemoryBuffer::builder()
            .max_total_size(1024 * 1024)
            .max_segment_size(256)
            .build()
            .unwrap();

        let mut recorder = recorder(writer)
            .source(PanickingFlushSource("panicking_flush_a"))
            .source(PanickingFlushSource("panicking_flush_b"))
            .on_recording_thread_start(move || {
                // Scoped to just the flush thread: a global subscriber would
                // collide with other tests' tracing state.
                let guard = tracing::subscriber::set_default(SourceWarnSubscriber {
                    warned_sources: subscriber_source.clone(),
                });
                move || drop(guard)
            })
            .build();
        recorder.handle().enable();

        // Give the flush thread time to run at least one cycle: both
        // sources panic on their very first flush.
        std::thread::sleep(Duration::from_millis(200));
        recorder.stop_flush_thread();

        let warned_sources = warned_sources.lock().unwrap();
        assert!(
            warned_sources.iter().any(|s| s == "panicking_flush_a"),
            "expected a warning naming panicking_flush_a, got {warned_sources:?}"
        );
        assert!(
            warned_sources.iter().any(|s| s == "panicking_flush_b"),
            "expected a warning naming panicking_flush_b \u{2014} it must not be suppressed by \
             panicking_flush_a's rate limit, got {warned_sources:?}"
        );
        // Both sources panic on every one of the ~40 cycles in the sleep
        // window above; more than one warning per source would mean the
        // 60s per-key rate limit isn't suppressing repeats.
        assert_eq!(
            warned_sources.len(),
            2,
            "expected exactly one warning per source (rate limit should suppress repeats \
             within the 60s window), got {warned_sources:?}"
        );
    }

    /// Same property as `distinct_panicking_flush_sources_are_each_warned_about`,
    /// for the `segment_metadata` call site.
    #[test]
    fn distinct_panicking_metadata_sources_are_each_warned_about() {
        let warned_sources = Arc::new(Mutex::new(Vec::new()));
        let subscriber_source = warned_sources.clone();

        let writer = MemoryBuffer::builder()
            .max_total_size(1024 * 1024)
            .max_segment_size(256)
            .build()
            .unwrap();

        let mut recorder = recorder(writer)
            .source(PanickingMetadataSource("panicking_metadata_a"))
            .source(PanickingMetadataSource("panicking_metadata_b"))
            .on_recording_thread_start(move || {
                // Scoped to just the flush thread: a global subscriber would
                // collide with other tests' tracing state.
                let guard = tracing::subscriber::set_default(SourceWarnSubscriber {
                    warned_sources: subscriber_source.clone(),
                });
                move || drop(guard)
            })
            .build();
        recorder.handle().enable();

        std::thread::sleep(Duration::from_millis(200));
        recorder.stop_flush_thread();

        let warned_sources = warned_sources.lock().unwrap();
        assert!(
            warned_sources.iter().any(|s| s == "panicking_metadata_a"),
            "expected a warning naming panicking_metadata_a, got {warned_sources:?}"
        );
        assert!(
            warned_sources.iter().any(|s| s == "panicking_metadata_b"),
            "expected a warning naming panicking_metadata_b \u{2014} it must not be suppressed by \
             panicking_metadata_a's rate limit, got {warned_sources:?}"
        );
        // Both sources panic on every one of the ~40 cycles in the sleep
        // window above; more than one warning per source would mean the
        // 60s per-key rate limit isn't suppressing repeats.
        assert_eq!(
            warned_sources.len(),
            2,
            "expected exactly one warning per source (rate limit should suppress repeats \
             within the 60s window), got {warned_sources:?}"
        );
    }
}
