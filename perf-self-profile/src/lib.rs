//! # perf-self-profile
//!
//! Minimal crate for a program to capture its own perf events with stack traces
//! using Linux `perf_event_open()`.
//!
//! This crate relies on `perf_event_paranoid <= 2`.
//!
//! Uses kernel frame-pointer-based stack walking
//! (`PERF_SAMPLE_CALLCHAIN`), so your binary must be compiled with frame pointers:
//!
//! ```toml
//! # Cargo.toml or .cargo/config.toml
//! [profile.release]
//! debug = true
//!
//! # In .cargo/config.toml:
//! [build]
//! rustflags = ["-C", "force-frame-pointers=yes"]
//! ```
//!
//! ## Quick start
//!
//! ```no_run
//! use dial9_perf_self_profile::{PerfSampler, SamplerConfig, SamplingMode, EventSource, Sample};
//!
//! let mut sampler = PerfSampler::start(
//!     SamplerConfig::default()
//!         .event_source(EventSource::SwCpuClock)
//!         .sampling(SamplingMode::FrequencyHz(999)),
//! ).expect("failed to start sampler");
//!
//! // ... do work ...
//!
//! // Drain samples
//! sampler.for_each_sample(|sample: &Sample| {
//!     println!("ip={:#x} callchain={} frames", sample.ip, sample.callchain.len());
//! });
//! ```
//!
//! ## Memory profiling
//!
//! With the `memory-profiling` feature, the `memory_profiling` module adds a
//! sampled allocation profiler: set `Dial9Allocator` as the global allocator
//! and register it with a dial9 recorder via `MemoryProfiler::install`.
//! Sampled allocations land in the trace as `AllocEvent`/`FreeEvent`.
//!
//! ```ignore
//! use dial9_perf_self_profile::memory_profiling::{Dial9Allocator, MemoryProfiler};
//!
//! #[global_allocator]
//! static ALLOC: Dial9Allocator = Dial9Allocator::system();
//!
//! // `handle` is a dial9 handle (dial9_core::handle::Dial9Handle).
//! MemoryProfiler::with_defaults().install(handle)?;
//! ```

pub mod offline_symbolize;
mod sampler;
mod symbolize;
mod sys;
pub mod tracepoint;
pub mod unwinder;

#[cfg(feature = "cpu-profiling")]
pub mod cpu_source;

#[cfg(any(
    feature = "cpu-profiling",
    feature = "memory-profiling",
    feature = "process-resource",
    feature = "linux-socket"
))]
pub mod recorder_ext;

#[cfg(feature = "process-resource")]
pub mod process_resource;

#[cfg(feature = "linux-socket")]
pub mod socket_accept_queues;

#[cfg(feature = "memory-profiling")]
pub mod memory_profiling;

#[cfg(feature = "symbolize-processor")]
pub mod symbolize_processor;

pub use offline_symbolize::SymbolTableEntry;
pub use sampler::{EventSource, Sample, SamplerConfig, SamplingMode};
pub use symbolize::{CodeInfo, MapsEntry, SymbolInfo};
pub use symbolize::{parse_proc_maps, read_proc_maps};

// Platform-dispatched re-exports
pub use sys::PerfSampler;
pub use sys::resolve_symbol;

// ctimer fallback status and thread registration
#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
pub use sys::is_ctimer_active;
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
)))]
pub fn is_ctimer_active() -> bool {
    false
}

/// Register the calling thread with the active profiling backend.
///
/// No-op unless ctimer fallback is active (perf uses `inherit` instead).
pub fn register_current_thread() -> Result<(), std::io::Error> {
    #[cfg(any(
        target_os = "linux",
        all(target_os = "android", target_arch = "aarch64")
    ))]
    if is_ctimer_active() {
        return crate::sys::fp_profiler::ctimer::register_thread();
    }
    Ok(())
}

/// Unregister the calling thread from the active profiling backend.
///
/// No-op unless ctimer fallback is active.
pub fn unregister_current_thread() {
    #[cfg(any(
        target_os = "linux",
        all(target_os = "android", target_arch = "aarch64")
    ))]
    if is_ctimer_active() {
        crate::sys::fp_profiler::ctimer::unregister_thread();
    }
}

// blazesym-dependent APIs
#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
pub use sys::{resolve_symbol_with_maps, resolve_symbols_with_maps};

#[cfg(feature = "cpu-profiling")]
pub use cpu_source::{
    CpuProfiler, CpuProfilingConfig, CpuSampleSource, SchedEventConfig, SchedProfiler,
};

#[cfg(any(
    feature = "cpu-profiling",
    feature = "memory-profiling",
    feature = "process-resource",
    feature = "linux-socket"
))]
pub use recorder_ext::RecorderPerfExt;

#[cfg(all(feature = "process-resource", unix))]
pub use process_resource::ProcessResourceUsageSource;
#[cfg(feature = "process-resource")]
pub use process_resource::{ProcessResourceUsageConfig, ProcessResourceUsageEvent};

#[cfg(all(feature = "linux-socket", target_os = "linux"))]
pub use socket_accept_queues::SocketAcceptQueuesSource;
#[cfg(feature = "linux-socket")]
pub use socket_accept_queues::{SocketAcceptQueuesConfig, TcpAcceptQueueEvent};

#[cfg(feature = "memory-profiling")]
pub use memory_profiling::{
    AllocEvent, DEFAULT_RING_CAPACITY, DEFAULT_SAMPLE_RATE_BYTES, Dial9Allocator, FreeEvent,
    InstallError, MemoryProfiler, MemoryProfilerGuard, MemoryProfilingConfig, is_installed,
};

#[cfg(feature = "symbolize-processor")]
pub use symbolize_processor::SymbolizeProcessor;

/// Internal module exposed only for benchmarks. Not part of the public API.
#[cfg(all(
    any(
        target_os = "linux",
        all(target_os = "android", target_arch = "aarch64")
    ),
    feature = "__internal-bench"
))]
#[doc(hidden)]
pub mod __bench_internals {
    pub use crate::sys::fp_profiler::install_handler;
    pub use crate::sys::fp_profiler::unwind::unwind;
}
