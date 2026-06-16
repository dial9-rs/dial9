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
pub(crate) mod buffer;
pub(crate) mod collector;
pub use collector::Batch;
#[cfg(feature = "cpu-profiling")]
pub mod cpu_profile;
pub(crate) mod custom_events;
pub(crate) mod events;
pub(crate) mod format;
pub(crate) mod process_resource_usage;
pub(crate) mod recorder;
pub mod task_dump_config;
pub(crate) mod task_metadata;
pub(crate) mod writer;

pub use crate::traced::TracedFuture;
pub use buffer::{Encodable, ThreadLocalEncoder};
pub use custom_events::{CustomEventsConfig, CustomEventsContext};
pub use events::{CpuSampleSource, clock_monotonic_ns};
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
pub use task_dump_config::TaskDumpConfig;
pub use task_metadata::{TaskId, UNKNOWN_TASK_ID};
pub use writer::{Disk, DiskWriter, InMemoryWriter, Memory, SegmentWriter, WriterMode};
