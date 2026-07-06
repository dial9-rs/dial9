#![deny(clippy::arithmetic_side_effects)]
//! Two lock-free MPMC queues — one for sampled allocations, one for frees.

use crossbeam_queue::ArrayQueue;
use dial9_core::primitives::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Default maximum frames captured per allocation. 128 × 8 B = 1 KiB stack budget.
pub(crate) const DEFAULT_MAX_FRAMES: usize = 128;
const _: () = assert!(
    DEFAULT_MAX_FRAMES <= u8::MAX as usize,
    "DEFAULT_MAX_FRAMES must fit in u8 (used as RawAlloc::frame_count)"
);

/// Default number of `RawFree` slots. 8× the alloc queue
/// ([`crate::memory_profiling::DEFAULT_RING_CAPACITY`]) — frees are smaller
/// than allocs (no stack frames), so the budget is asymmetric in their favour
/// to absorb burst free traffic without dropping. The actual capacity is
/// derived from `MemoryProfilingConfig::ring_capacity()` in
/// [`crate::memory_profiling::profiler::MemoryProfiler::install`]; this const
/// exists to document the 8× relationship in one place.
#[expect(
    dead_code,
    reason = "documents the 8x sizing rationale referenced from MemoryProfiler::install"
)]
pub(crate) const DEFAULT_FREE_QUEUE_CAPACITY: usize =
    crate::memory_profiling::config::DEFAULT_RING_CAPACITY * 8;

/// One sampled allocation captured on the producer thread.
///
/// The inline stack buffer holds `DEFAULT_MAX_FRAMES` entries (128),
/// giving a 1 KiB stack budget for the frames field.
#[derive(Debug, Clone)]
pub(crate) struct RawAlloc {
    pub(crate) tid: u32,
    pub(crate) size: u64,
    pub(crate) addr: u64,
    pub(crate) ts_ns: u64,
    pub(crate) frames: [u64; DEFAULT_MAX_FRAMES],
    pub(crate) frame_count: u8,
}

impl RawAlloc {
    #[expect(dead_code, reason = "available for future consumers of RawAlloc")]
    pub(crate) fn frames(&self) -> &[u64] {
        let count = (self.frame_count as usize).min(DEFAULT_MAX_FRAMES);
        &self.frames[..count]
    }
}

/// One free captured on the producer thread when liveset tracking is on.
///
/// With the producer-side liveset, `size` and `alloc_ts_ns` are denormalized
/// from the liveset entry so the consolidator can emit `FreeEvent` directly
/// without a second lookup.
///
/// **Shutdown drain.** When `on_dealloc` fires on a thread that is in TLS
/// teardown, the OPT_OUT sentinel forbids the producer from touching the
/// `scc::HashIndex` (`sdd`'s TLS may be destroyed). In that case the
/// producer pushes a `RawFree { shutdown: true, .. }` carrying only `addr`
/// (size and alloc_ts_ns are zero — placeholders). The consolidator does
/// the liveset peek/remove on its own healthy thread; see
/// `MemoryProfileSource::handle_free`. This recovers the dying thread's
/// `FreeEvent`s and bounds liveset growth, at the cost of a small
/// race window when an address is reused before the consolidator drains.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawFree {
    pub(crate) tid: u32,
    pub(crate) addr: u64,
    pub(crate) ts_ns: u64,
    /// Size of the original sampled allocation (denormalized from liveset).
    /// `0` when `shutdown == true` (the producer couldn't peek the liveset
    /// safely; the consolidator fills this in from the lookup it does).
    pub(crate) size: u64,
    /// Timestamp of the original sampled allocation (denormalized from
    /// liveset). `0` when `shutdown == true`, same reason as `size`.
    pub(crate) alloc_ts_ns: u64,
    /// `true` if pushed from a thread in TLS teardown; the consolidator
    /// must do the liveset peek/remove because the producer couldn't.
    /// See struct-level docs for the protocol.
    pub(crate) shutdown: bool,
}

/// Process-global pair of lock-free queues for the memory profiler.
///
/// Producers (allocator hook) and the consumer (`MemoryProfileSource`) both
/// hold `Arc<RingBuffers>` and access the queues via `&self` — no inner
/// `Arc`s, so the `&Arc<...>` smell is contained to the outer borrow.
pub(crate) struct RingBuffers {
    pub(crate) alloc_queue: ArrayQueue<RawAlloc>,
    pub(crate) free_queue: ArrayQueue<RawFree>,
    pub(crate) dropped_allocs: AtomicU64,
    pub(crate) dropped_frees: AtomicU64,
}

impl RingBuffers {
    pub(crate) fn new(alloc_capacity: usize, free_capacity: usize) -> Self {
        Self {
            alloc_queue: ArrayQueue::new(alloc_capacity),
            free_queue: ArrayQueue::new(free_capacity),
            dropped_allocs: AtomicU64::new(0),
            dropped_frees: AtomicU64::new(0),
        }
    }

    /// Push a sampled allocation, incrementing the drop counter on overflow.
    ///
    /// Allocation-free: only `ArrayQueue::push` (lock-free CAS) +
    /// `AtomicU64::fetch_add`.
    pub(crate) fn push_alloc(&self, sample: RawAlloc) {
        if self.alloc_queue.push(sample).is_err() {
            self.dropped_allocs.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Push a free record, incrementing the drop counter on overflow.
    ///
    /// Allocation-free: only `ArrayQueue::push` (lock-free CAS) +
    /// `AtomicU64::fetch_add`.
    pub(crate) fn push_free(&self, sample: RawFree) {
        if self.free_queue.push(sample).is_err() {
            self.dropped_frees.fetch_add(1, Ordering::Relaxed);
        }
    }
}
