//! Operational metrics for the core flush path, published via metrique.

use metrique::unit_of_work::metrics;

/// Per-cycle counters returned by [`SharedState::drain_all_tl_buffers`], also
/// used as a `#[metrics(subfield)]` so callers can flatten these fields into
/// their top-level metrics without duplication.
///
/// [`SharedState::drain_all_tl_buffers`]: crate::shared_state::SharedState::drain_all_tl_buffers
#[doc(hidden)]
#[metrics(subfield)]
#[derive(Debug, Default)]
pub struct TlDrainStats {
    /// Buffers that we locked cross-thread and had pending events.
    pub buffers_flushed: u64,
    /// Buffers that we locked cross-thread (superset of `buffers_flushed`;
    /// the difference is buffers that were already empty when locked).
    pub buffers_locked: u64,
    /// Handles skipped because the owning thread self-flushed during the
    /// epoch grace period. High ratio means busy workers are self-flushing
    /// efficiently and the intrusive path is staying out of their way.
    pub buffers_skipped_busy: u64,
    /// Total events drained from idle/silent buffers this cycle.
    pub events_flushed: u64,
    /// Dead `Weak` handles pruned this cycle (threads that have exited).
    pub dead_pruned: u64,
}
