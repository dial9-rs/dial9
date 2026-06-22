//! Source trait for abstracting flush-thread data sources.

use crate::buffer::{self, Encodable, ThreadLocalEncoder};
use crate::collector::CentralCollector;
use crate::primitives::sync::Arc;
use crate::primitives::sync::atomic::AtomicU64;

/// Context passed to [`Source::flush`] for recording events into the trace.
///
/// Use [`record_event`] to emit an encodable event, or [`with_encoder`] when
/// you need direct encoder access (e.g. to intern stack frames).
///
/// [`record_event`]: FlushContext::record_event
/// [`with_encoder`]: FlushContext::with_encoder
pub struct FlushContext<'a> {
    collector: &'a Arc<CentralCollector>,
    drain_epoch: &'a AtomicU64,
}

impl<'a> FlushContext<'a> {
    pub(crate) fn new(collector: &'a Arc<CentralCollector>, drain_epoch: &'a AtomicU64) -> Self {
        Self {
            collector,
            drain_epoch,
        }
    }

    /// Record an event into the trace from this flush cycle.
    pub fn record_event(&self, event: &dyn Encodable) {
        let _ = buffer::record_encodable_event(event, self.collector, self.drain_epoch);
    }

    /// Record via the thread-local encoder directly.
    ///
    /// Use this when you need encoder-level access, e.g. to call
    /// `enc.intern_stack_frames(..)` before encoding an event.
    pub fn with_encoder(&self, f: impl FnOnce(&mut ThreadLocalEncoder<'_>)) {
        let _ = buffer::with_encoder(f, self.collector, self.drain_epoch);
    }
}

/// A data source drained by the flush thread each cycle.
///
/// Implement this trait to feed custom events into the dial9 trace. Register
/// the source with [`SharedState::push_source`] before starting the flush
/// thread; the flush thread calls [`flush`] once per cycle.
///
/// [`flush`]: Source::flush
/// [`SharedState::push_source`]: crate::shared_state::SharedState::push_source
pub trait Source: Send {
    /// Drain pending data into the trace. Called once per flush cycle.
    fn flush(&mut self, ctx: &FlushContext<'_>);

    /// Diagnostic name for this source (e.g. `"cpu_profile"`, `"sched"`).
    fn name(&self) -> &'static str;

    /// Called when a worker thread starts.
    ///
    /// Per-thread sources (e.g. `SchedProfiler`) use this to begin tracking
    /// the current thread. Returns an error if setup fails.
    fn on_worker_thread_start(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    /// Called when a thread stops. Per-thread sources use this to stop
    /// tracking the current thread.
    fn on_thread_stop(&mut self) {}

    /// Key-value entries this source contributes to segment metadata.
    /// Merged into the next rotated segment each flush cycle.
    fn segment_metadata(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}
