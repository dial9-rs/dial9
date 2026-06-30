//! Memory profiling — sampled allocation tracking via ring buffers.
//!
//! The architecture:
//!
//! 1. The allocator hook ([`SamplingAllocator`]) does the bare minimum on the
//!    allocating thread: sampling decision, stack capture, push a
//!    fixed-size POD record into one of two process-global lock-free
//!    queues.
//! 2. The consumer ([`MemorySampler`], returned by [`MemoryProfiler::install`])
//!    drains both queues, interns stacks, and yields [`MemorySample`]s. The
//!    dial9 integration (the `dial9-source` feature) wraps it in a `Source`
//!    that emits `AllocEvent`s and `FreeEvent`s.
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
//! The base profiler needs no dial9 dependency. Gated behind the
//! `memory-profiling` cargo feature; the `dial9-source` feature adds the
//! `Source` integration.

mod allocator;
mod clock;
mod config;
mod hook;
mod opt_out;
mod profiler;
mod ring;
mod sampling;
mod tid;

#[cfg(feature = "dial9-source")]
mod events;
#[cfg(feature = "dial9-source")]
mod source;

pub use allocator::SamplingAllocator;
pub use config::{DEFAULT_RING_CAPACITY, DEFAULT_SAMPLE_RATE_BYTES, MemoryProfilingConfig};
#[cfg(feature = "test-util")]
pub use profiler::push_test_alloc;
pub use profiler::{
    AllocSample, DroppedSamples, FreeSample, InstallError, MemoryProfiler, MemorySample,
    MemorySampler, is_installed,
};

#[cfg(feature = "dial9-source")]
pub use events::{AllocEvent, FreeEvent};
#[cfg(feature = "dial9-source")]
pub use source::MemoryProfileSource;
