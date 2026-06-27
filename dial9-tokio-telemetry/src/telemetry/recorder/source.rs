//! Source trait for abstracting flush-thread data sources.

use crate::primitives::sync::Arc;
use crate::primitives::sync::atomic::AtomicU64;
use crate::telemetry::collector::CentralCollector;

/// Context passed to [`Source::flush`] containing shared state needed for draining.
pub(crate) struct FlushContext<'a> {
    pub collector: &'a Arc<CentralCollector>,
    pub drain_epoch: &'a AtomicU64,
}

/// A data source that the flush thread drains into the central collector.
///
/// Implementors (e.g. `CpuProfiler`, `SchedProfiler`, `TokioRuntimesSource`)
/// provide a `flush` method that drains pending data and records it via
/// `record_encodable_event`.
pub(crate) trait Source: Send {
    /// Drain pending data into the dial9 trace. Called once per flush cycle
    /// from the flush thread.
    fn flush(&mut self, ctx: &FlushContext<'_>);

    /// Diagnostic name (e.g. "cpu_profile", "sched").
    fn name(&self) -> &'static str;

    /// Called when a worker thread starts. Used by per-thread sources like SchedProfiler
    /// to start tracking the current thread. Returns an error if tracking fails.
    fn on_worker_thread_start(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    /// Called when a thread stops. Used by per-thread sources like SchedProfiler
    /// to stop tracking the current thread.
    fn on_thread_stop(&mut self) {}

    /// Append this source's segment-metadata entries to `out` **iff** they have
    /// changed since the last call.
    ///
    /// Appending nothing on an unchanged cycle is what lets the flush loop skip
    /// the merge (it merges only when `out` is non-empty), so steady-state
    /// cycles allocate nothing. The default reports a source with no metadata.
    fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
        let _ = out;
    }
}

/// Collect current segment metadata from every source by calling the
/// change-aware [`Source::segment_metadata`] once each. A freshly-built source
/// reports its metadata on the first call, so this yields the full set.
#[cfg(test)]
pub(crate) fn collect_segment_metadata(sources: &mut [Box<dyn Source>]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for source in sources.iter_mut() {
        source.segment_metadata(&mut out);
    }
    out
}
