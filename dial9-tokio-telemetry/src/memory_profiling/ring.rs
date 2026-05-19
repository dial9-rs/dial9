//! Two lock-free MPMC queues — one for sampled allocations, one for frees.

use crate::primitives::sync::atomic::AtomicU64;
use crossbeam_queue::ArrayQueue;

/// Default maximum frames captured per allocation. 128 × 8 B = 1 KiB stack budget.
pub(crate) const DEFAULT_MAX_FRAMES: usize = 128;

/// Default number of `RawAlloc` slots. ~4 MiB total at 128 frames (design §5).
pub(crate) const DEFAULT_ALLOC_QUEUE_CAPACITY: usize = 4096;

/// Default number of `RawFree` slots. 8× the alloc queue (design §9).
#[expect(dead_code, reason = "wired up by allocator hook in a later commit")]
pub(crate) const DEFAULT_FREE_QUEUE_CAPACITY: usize = DEFAULT_ALLOC_QUEUE_CAPACITY * 8;

/// One sampled allocation captured on the producer thread.
///
/// `MAX_FRAMES` controls the size of the inline stack buffer. The default
/// (128) gives a 1 KiB stack budget for the frames field.
#[derive(Debug, Clone)]
pub(crate) struct RawAlloc<const MAX_FRAMES: usize = DEFAULT_MAX_FRAMES> {
    pub(crate) tid: u32,
    pub(crate) size: u64,
    pub(crate) addr: u64,
    pub(crate) ts_ns: u64,
    pub(crate) frames: [u64; MAX_FRAMES],
    pub(crate) frame_count: u8,
}

impl<const MAX_FRAMES: usize> RawAlloc<MAX_FRAMES> {
    #[expect(dead_code, reason = "used by allocator hook in a later commit")]
    pub(crate) fn frames(&self) -> &[u64] {
        const { assert!(MAX_FRAMES <= u8::MAX as usize, "MAX_FRAMES must fit in u8") };
        &self.frames[..self.frame_count as usize]
    }
}

/// One free captured on the producer thread when liveset tracking is on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawFree {
    pub(crate) tid: u32,
    pub(crate) addr: u64,
    #[expect(
        dead_code,
        reason = "consolidator uses size from the liveset entry, not from RawFree; the field is here so the producer doesn't have to do a separate lookup"
    )]
    pub(crate) size: u64,
    pub(crate) ts_ns: u64,
}

/// Process-global pair of lock-free queues for the memory profiler.
///
/// Producers (allocator hook) and the consumer (`MemoryProfileSource`) both
/// hold `Arc<RingBuffers<N>>` and access the queues via `&self` — no inner
/// `Arc`s, so the `&Arc<...>` smell is contained to the outer borrow.
pub(crate) struct RingBuffers<const MAX_FRAMES: usize = DEFAULT_MAX_FRAMES> {
    pub(crate) alloc_queue: ArrayQueue<RawAlloc<MAX_FRAMES>>,
    pub(crate) free_queue: ArrayQueue<RawFree>,
    #[expect(dead_code, reason = "incremented by allocator hook in a later commit")]
    pub(crate) dropped_allocs: AtomicU64,
    #[expect(dead_code, reason = "incremented by allocator hook in a later commit")]
    pub(crate) dropped_frees: AtomicU64,
}

// `RingBuffers::new` is only called from tests in this commit. The allocator
// hook in a later commit will call it from a non-test path; at that point
// this `allow(dead_code)` becomes inert.
#[cfg_attr(not(test), allow(dead_code))]
impl<const MAX_FRAMES: usize> RingBuffers<MAX_FRAMES> {
    pub(crate) fn new(alloc_capacity: usize, free_capacity: usize) -> Self {
        Self {
            alloc_queue: ArrayQueue::new(alloc_capacity),
            free_queue: ArrayQueue::new(free_capacity),
            dropped_allocs: AtomicU64::new(0),
            dropped_frees: AtomicU64::new(0),
        }
    }
}
