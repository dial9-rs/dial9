#![doc = include_str!("../README.md")]
#![warn(
    missing_debug_implementations,
    missing_docs,
    rust_2018_idioms,
    unreachable_pub
)]
#![deny(unused_must_use, unsafe_op_in_unsafe_fn)]

/// Background worker pipeline for processing sealed trace segments.
pub mod background_task;
/// On-trigger pipeline runs: dump triggers, receipts, and ids.
pub use dial9_core::dump;
/// Sampled memory allocation profiling.
#[cfg(feature = "memory-profiling")]
pub mod memory_profiling {
    pub use dial9_perf_self_profile::memory_profiling::*;
}
#[cfg(feature = "taskdump")]
pub(crate) use dial9_core::sampling;
pub(crate) use dial9_core::{primitives, rate_limit};
#[cfg(feature = "taskdump")]
pub(crate) mod task_dumped;
/// Core telemetry types, recording, and trace I/O.
pub mod telemetry;
pub(crate) mod traced;
#[cfg(feature = "taskdump")]
pub(crate) mod unwind;

pub use telemetry::{TracedFuture, block_on, spawn, spawn_in};
