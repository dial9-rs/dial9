//! Core telemetry module.
//!
//! All public types are re-exported here — use `dial9_tokio_telemetry::telemetry::*`
//! rather than reaching into sub-modules.

#[cfg(any(test, feature = "analysis"))]
/// Trace file reading and analysis utilities.
pub mod analysis;
/// Decode-side companion structs for built-in trace events.
#[cfg(any(feature = "analysis", test))]
pub mod analysis_events;
pub(crate) use dial9_core::buffer;
pub(crate) use dial9_core::custom_events;
pub(crate) mod events;
pub(crate) mod format;
pub(crate) mod recorder;
pub mod task_dump_config;
pub(crate) mod task_metadata;
pub(crate) use dial9_core::writer;

pub use crate::traced::TracedFuture;
pub use custom_events::{CustomEventsConfig, CustomEventsContext};
pub use dial9_core::buffer::{Encodable, ThreadLocalEncoder};
pub use dial9_core::recorder::{RecorderBuilder, recorder};
#[cfg(any(
    feature = "cpu-profiling",
    feature = "process-resource",
    feature = "linux-socket"
))]
pub use dial9_perf_self_profile::RecorderPerfExt;
#[cfg(feature = "linux-socket")]
pub use dial9_perf_self_profile::SocketAcceptQueuesConfig;
#[cfg(feature = "memory-profiling")]
pub use dial9_perf_self_profile::{AllocEvent, FreeEvent};
#[cfg(feature = "cpu-profiling")]
pub use dial9_perf_self_profile::{
    CpuProfiler, CpuProfilingConfig, CpuSampleSource, SchedEventConfig, SchedProfiler,
};
#[cfg(feature = "process-resource")]
pub use dial9_perf_self_profile::{ProcessResourceUsageConfig, ProcessResourceUsageEvent};
pub use events::clock_monotonic_ns;
pub use format::{
    PollEndEvent, PollStartEvent, TaskSpawnEvent, WakeEventEvent, WorkerId, WorkerParkEvent,
    WorkerUnparkEvent,
};
pub use recorder::{
    Dial9Handle, Dial9TokioHandle, RecorderBuilderTokioExt, TelemetryCore, TelemetryCoreBuilder,
    TelemetryGuard, TokioAttachConfig, TokioHooks, TraceRuntimeCoreBuilder, TracedRecorder,
    TracedRuntime, build_traced, current_worker_id, spawn,
};
pub use task_dump_config::TaskDumpConfig;
pub use task_metadata::{TaskId, UNKNOWN_TASK_ID};
pub use writer::{Disk, DiskWriter, InMemoryWriter, Memory, SegmentWriter, WriterMode};

/// Install the memory profiler on a running session (post-`enable`).
/// Warns and skips on error or a disabled guard.
#[cfg(feature = "memory-profiling")]
pub(crate) fn install_memory_profiler_on_guard(
    config: Option<dial9_perf_self_profile::memory_profiling::MemoryProfilingConfig>,
    guard: &TelemetryGuard,
) -> Option<dial9_perf_self_profile::memory_profiling::MemoryProfilerGuard> {
    let config = config?;
    if !guard.is_enabled() {
        return None;
    }
    match dial9_perf_self_profile::memory_profiling::MemoryProfiler::from_config(config)
        .install(guard.handle())
    {
        Ok(memory_guard) => Some(memory_guard),
        Err(e) => {
            tracing::warn!(target: "dial9_telemetry", "failed to install memory profiler: {e}");
            None
        }
    }
}
