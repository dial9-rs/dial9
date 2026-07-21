use crate::buffer::{BufferMode, SegmentWriter};
use crate::flush_loop::run_flush_loop;
use crate::handle::{ControlCommand, Dial9Handle};
use crate::primitives::sync::{Arc, Mutex};
use crate::primitives::{sync::mpsc, thread::JoinHandle};
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
    #[cfg(feature = "pipeline")]
    worker: Option<WorkerHandle>,
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
            #[cfg(feature = "pipeline")]
            worker: None,
        }
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

    /// Attach the background worker to this recorder, so its lifecycle is tied
    /// to the recorder's (drained on `graceful_shutdown`, stopped on drop).
    #[cfg(feature = "pipeline")]
    pub(crate) fn attach_worker(&mut self, worker: WorkerHandle) {
        self.worker = Some(worker);
    }

    /// The shared recording state.
    pub fn shared(&self) -> Option<&Arc<SharedState>> {
        self.handle.shared()
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
    }

    /// Flush remaining events, seal the final segment, and (with `pipeline`)
    /// wait for the background worker to drain within `timeout`.
    ///
    /// Call this after any runtime that owns worker threads has been dropped, so
    /// their thread-local buffers have already been flushed. Consumes the
    /// recorder so `Drop` becomes a no-op.
    pub fn graceful_shutdown(mut self, timeout: Duration) -> std::io::Result<()> {
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

        Ok(())
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
