//! Memory profiling — sampled allocation tracking via ring buffers.
//!
//! The architecture:
//!
//! 1. The allocator hook ([`Dial9Allocator`]) does the bare minimum on the
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

mod allocator;
mod config;
mod hook;
mod profiler;
mod ring;
mod source;

pub use allocator::Dial9Allocator;
pub use config::{
    DEFAULT_RING_CAPACITY, DEFAULT_SAMPLE_RATE_BYTES, MemoryProfilingConfig, TimestampMode,
};
#[cfg(feature = "analysis")]
pub use profiler::push_test_alloc;
pub use profiler::{InstallError, MemoryProfiler, MemoryProfilerGuard, is_installed};
