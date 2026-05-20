//! Trace analysis utilities (**unstable**).
//!
//! This module is feature-gated behind `analysis` and re-exports every public
//! type and function from the internal `telemetry::analysis` implementation.
//!
//! The API surface here is still evolving.
//! Expect breaking changes between minor versions.
//!
//! # Memory profiling: scaling sampled `Alloc` events
//!
//! `Alloc` events in the trace are sampled with Poisson sampling at the
//! configured `MemoryProfilingConfig::sample_rate_bytes` (`R`). The
//! `size` field on each event is the raw bytes of *that one* allocation
//! — it is **not** a scaled estimate. To recover unbiased totals from
//! the sample stream:
//!
//! ```text
//! total_bytes ≈ Σ s_i / (1 - exp(-s_i / R))
//! total_count ≈ Σ   1 / (1 - exp(-s_i / R))
//! ```
//!
//! Always weight each sample individually before summing. See
//! `MemoryProfilingConfig::sample_rate_bytes` and
//! `docs/design/memory-profiling.md` for worked examples.
//!
//! # Quick start
//!
//! ```no_run
//! # fn main() -> std::io::Result<()> {
//! use dial9_tokio_telemetry::analysis_unstable::{TraceReader, analyze_trace, print_analysis};
//!
//! let reader = TraceReader::new("trace.0.bin")?;
//! let analysis = analyze_trace(&reader.runtime_events);
//! print_analysis(&analysis, &reader.spawn_locations);
//! # Ok(())
//! # }
//! ```

pub use crate::telemetry::analysis::{
    ActivePeriod, LongPoll, SampledPoll, SchedDelay, SpawnLocationStats, TraceAnalysis,
    TraceReader, WakeDelay, WorkerStats, analyze_trace, compute_active_periods,
    compute_wake_to_poll_delays, detect_idle_workers, detect_long_polls, detect_sampled_polls,
    detect_sched_delays, detect_wake_delays, print_analysis,
};
pub use crate::telemetry::format::decode_events;
