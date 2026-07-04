//! Wire-format events emitted by the memory profiler consolidator.

use dial9_trace_format::{InternedStackFrames, TraceEvent};

/// Wire-format event for a sampled memory allocation.
///
/// Emitted from the consolidator (flush thread) for allocations that tripped
/// the geometric sampling counter. The sampling rate that produced this event
/// lives in the segment metadata, not on each event.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
#[cfg_attr(not(feature = "unstable-events"), non_exhaustive)]
pub struct AllocEvent {
    /// Wall-clock timestamp in nanoseconds (monotonic).
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// OS thread ID of the allocating thread. Same source as `WorkerParkEvent.tid`
    /// and `CpuSampleEvent.tid`. Use this to join against worker park/unpark
    /// history to recover worker_id when the allocation happened on a tokio
    /// worker thread.
    pub tid: u32,
    /// Allocation size in bytes. The actual size requested by the allocating
    /// code; the underlying allocator may have rounded up, but that's not
    /// recorded here.
    pub size: u64,
    /// Returned pointer. Always the actual address returned by the allocator;
    /// it is only matched against `FreeEvent.addr` when liveset tracking is
    /// on. Consumers should not assume cross-allocation uniqueness when
    /// liveset is off — addresses are reused freely once the slot is freed,
    /// and without paired frees you cannot tell which "generation" of the
    /// address a given event belongs to.
    pub addr: u64,
    /// Stack at the allocation site. Frame 0 is the most-recent caller.
    pub callchain: InternedStackFrames,
}

/// Wire-format event for a deallocation paired with a previously-sampled
/// `AllocEvent`. Only emitted when liveset tracking is on.
///
/// `size` and `alloc_timestamp_ns` are denormalized from the matching
/// `AllocEvent` so the free stays analytically useful when the corresponding
/// `AllocEvent` has been evicted by trace rotation. See design §3
/// "Why denormalize size and alloc_timestamp_ns?" for the rationale.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
#[cfg_attr(not(feature = "unstable-events"), non_exhaustive)]
pub struct FreeEvent {
    /// Wall-clock timestamp in nanoseconds (monotonic) of the free.
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// OS thread ID of the freeing thread.
    pub tid: u32,
    /// Pointer that was freed. Matches a previously-seen `AllocEvent.addr`.
    pub addr: u64,
    /// Size of the allocation being freed. Denormalized from the matching
    /// `AllocEvent` for rotation robustness.
    pub size: u64,
    /// Monotonic-ns timestamp of the original `AllocEvent`. Allows leak
    /// analysis to bucket frees by generation without needing the
    /// `AllocEvent` in the same (unrotated) trace.
    pub alloc_timestamp_ns: u64,
}

/// Wire-format event emitted when the memory profiler's ring buffers
/// overflowed during a flush period. Each field is the delta (new drops
/// since the previous flush), not a cumulative total. Only emitted when
/// at least one counter is non-zero.
///
/// Dropped frees cause the liveset to retain addresses that were actually
/// freed, producing false positives in leak analysis.
#[derive(Debug, TraceEvent)]
#[traceevent(wire_slot)]
pub(crate) struct MemoryProfileOverflowEvent {
    #[traceevent(timestamp)]
    pub timestamp_ns: u64,
    /// Alloc samples dropped since last flush due to alloc queue overflow.
    pub dropped_allocs: u64,
    /// Free samples dropped since last flush due to free queue overflow.
    pub dropped_frees: u64,
}
