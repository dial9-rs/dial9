//! Memory profiling — sampled allocation tracking via ring buffers.
//!
//! See `docs/design/memory-profiling.md` for the full design. The
//! architecture (design §5, §6, §9):
//!
//! 1. The allocator hook (later commit) does the bare minimum on the
//!    allocating thread: sampling decision, stack capture, push a
//!    fixed-size POD record into one of two process-global lock-free
//!    queues.
//! 2. The flush thread (consolidator) drains both queues every flush cycle
//!    via the `Source` trait, interns stacks, and emits `AllocEvent`s and
//!    `FreeEvent`s into the central collector.
//!
//! ## Why two queues
//!
//! Allocs and frees have very different rates and record sizes:
//! - `RawAlloc` (~1 KiB at 128 frames) is pushed only on sampled
//!   allocations, ~2K/sec at default sample rate.
//! - `RawFree` (~32 B) is pushed on every dealloc when liveset tracking is
//!   on, potentially 15M/sec.
//!
//! A unified queue would either over-size the alloc queue or under-size
//! the free queue. Splitting the queues lets us size each independently:
//! at default capacities the alloc queue is ~4 MiB and the free queue is
//! ~1 MiB (8× the slot count of the alloc queue, but each slot is ~32× smaller).
//!
//! Gated behind the `memory-profiling` cargo feature.

mod ring;
mod source;

#[expect(
    unused_imports,
    reason = "wired up by allocator hook in a later commit"
)]
pub(crate) use ring::{
    DEFAULT_ALLOC_QUEUE_CAPACITY, DEFAULT_FREE_QUEUE_CAPACITY, DEFAULT_MAX_FRAMES, RawAlloc,
    RawFree, RingBuffers,
};
#[expect(
    unused_imports,
    reason = "wired up by allocator hook in a later commit"
)]
pub(crate) use source::MemoryProfileSource;
