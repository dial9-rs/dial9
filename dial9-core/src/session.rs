use crate::flush_loop::run_flush_loop;
use crate::handle::{ControlCommand, Dial9Handle};
use crate::primitives::{sync::mpsc, thread::JoinHandle};
use crate::writer::{SegmentWriter, WriterMode};

/// Owns a recording session: the [`Dial9Handle`] and the flush thread.
///
/// Create one with [`CoreSession::new`], then call [`stop_flush_thread`] to
/// flush remaining events, seal the final trace segment, and join the flush
/// thread before tearing down any higher-level runtime state.
///
/// [`stop_flush_thread`]: CoreSession::stop_flush_thread
pub struct CoreSession {
    handle: Dial9Handle,
    flush_thread: Option<JoinHandle<()>>,
}

impl CoreSession {
    /// Create a session from an existing handle and flush thread.
    pub fn new(handle: Dial9Handle, flush_thread: Option<JoinHandle<()>>) -> Self {
        Self {
            handle,
            flush_thread,
        }
    }

    /// Start a session: spawn the flush thread that drains the bus into
    /// `writer`, and own its lifecycle.
    ///
    /// `thread_init` runs once on the flush thread before the loop and returns
    /// a teardown closure run after it — use it to register/unregister the
    /// thread with a runtime's profiler. `handle` must be enabled.
    pub fn start<M, Init, Teardown>(
        handle: Dial9Handle,
        writer: SegmentWriter<M>,
        control_rx: mpsc::Receiver<ControlCommand>,
        flush_metrics_sink: metrique::writer::BoxEntrySink,
        thread_init: Init,
    ) -> Self
    where
        M: WriterMode + Send + 'static,
        Init: FnOnce() -> Teardown + Send + 'static,
        Teardown: FnOnce(),
    {
        let shared = handle
            .shared()
            .expect("CoreSession::start requires an enabled handle")
            .clone();
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

    /// The recording handle for this session.
    pub fn handle(&self) -> &Dial9Handle {
        &self.handle
    }

    /// Flush remaining events, seal the final segment, and join the flush thread.
    ///
    /// Call this before dropping any runtime state that owns worker threads, so
    /// that their thread-local buffers have already been flushed to the central
    /// collector.
    pub fn stop_flush_thread(&mut self) {
        // Drain the calling thread's local buffer — it won't get a thread-stop
        // hook, so any unflushed events would be lost otherwise.
        if let Some(shared) = self.handle.shared() {
            crate::buffer::drain_to_collector(&shared.collector);
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
}
