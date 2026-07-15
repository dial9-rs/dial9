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
pub(crate) use dial9_core::custom_events;
pub(crate) use dial9_core::encoder;
pub(crate) mod events;
pub(crate) mod format;
pub(crate) mod recorder;
pub mod task_dump_config;
pub(crate) mod task_metadata;
pub(crate) use dial9_core::buffer;

pub use crate::traced::TracedFuture;
pub use buffer::{BufferMode, Disk, DiskBuffer, Memory, MemoryBuffer, SegmentWriter};
pub use custom_events::{CustomEventsConfig, CustomEventsContext};
pub use dial9_core::encoder::{Encodable, ThreadLocalEncoder};
pub use dial9_core::recorder::{RecorderBuilder, recorder};
#[cfg(any(
    feature = "cpu-profiling",
    feature = "process-resource",
    feature = "linux-socket",
    feature = "memory-profiling"
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
    Dial9Handle, Dial9TokioHandle, RecorderBuilderTokioExt, TokioAttachConfig, TokioHooks,
    TracedRuntime, TracedRuntimeBuilder, build_traced, current_worker_id, spawn,
};
pub use task_dump_config::TaskDumpConfig;
pub use task_metadata::{TaskId, UNKNOWN_TASK_ID};
