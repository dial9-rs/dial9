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
pub(crate) mod custom_events;
pub(crate) mod events;
pub(crate) mod format;
pub(crate) mod process_resource_usage;
pub(crate) mod recorder;
#[cfg(feature = "linux-socket")]
pub(crate) mod socket_accept_queues;
pub mod task_dump_config;
pub(crate) mod task_metadata;
pub(crate) use dial9_core::writer;

pub use crate::traced::TracedFuture;
pub use custom_events::{CustomEventsConfig, CustomEventsContext};
pub use dial9_core::buffer::{Encodable, ThreadLocalEncoder};
#[cfg(feature = "cpu-profiling")]
pub use dial9_perf_self_profile::{
    CpuProfiler, CpuProfilingConfig, CpuSampleSource, SchedEventConfig, SchedProfiler,
};
pub use events::clock_monotonic_ns;
pub use format::{
    AllocEvent, FreeEvent, PollEndEvent, PollStartEvent, ProcessResourceUsageEvent, TaskSpawnEvent,
    WakeEventEvent, WorkerId, WorkerParkEvent, WorkerUnparkEvent,
};
pub use process_resource_usage::ProcessResourceUsageConfig;
pub use recorder::{
    BuildAndStartRuntime, Dial9Handle, Dial9TokioHandle, HasTracePath, NoTracePath, PipelineCustom,
    PipelineS3, PipelineUnset, TelemetryCore, TelemetryCoreBuilder, TelemetryGuard,
    TelemetryRuntimeError, TokioHooks, TraceRuntimeCoreBuilder, TracedRuntime,
    TracedRuntimeBuilder, current_worker_id, spawn,
};
#[cfg(feature = "linux-socket")]
pub use socket_accept_queues::SocketAcceptQueuesConfig;
pub use task_dump_config::TaskDumpConfig;
pub use task_metadata::{TaskId, UNKNOWN_TASK_ID};
pub use writer::{Disk, DiskWriter, InMemoryWriter, Memory, SegmentWriter, WriterMode};
