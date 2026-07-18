#![doc = include_str!("../README.md")]

// Core recording API
pub use dial9_core::buffer::{Disk, DiskBuffer, Memory, MemoryBuffer};
pub use dial9_core::handle::Dial9Handle;
pub use dial9_core::recorder::{RecorderBuilder, RecorderSourceExt, recorder};
pub use dial9_core::recording::Recorder;

/// Building blocks for extending dial9: implement a [`Source`](crate::core::Source),
/// write custom encoders, author custom segment processors, reach the raw
/// recording modules.
pub mod core {
    pub use dial9_core::buffer::{self, BufferMode, SegmentWriter};
    pub use dial9_core::clock::{self, clock_monotonic_ns};
    pub use dial9_core::custom_events::{CustomEventsConfig, CustomEventsContext};
    pub use dial9_core::encoder::{self, Encodable, ThreadLocalEncoder};
    pub use dial9_core::handle::{self, clear_tl_handle, current_handle, set_tl_handle};
    pub use dial9_core::recorder;
    pub use dial9_core::source::{self, FlushContext, Source};

    // Background pipeline (segment worker, on-demand dumps).
    #[cfg(feature = "pipeline")]
    pub use dial9_core::{dump, worker};

    /// Segment pipeline: processors, offline symbolization, and (with `tokio`)
    /// the pipeline builder, worker config, and S3 upload stage.
    #[cfg(feature = "pipeline")]
    pub mod pipeline {
        pub use dial9_core::pipeline::{
            MemorySegment, Payload, ProcessError, ProcessErrorKind, SealedSegment, SegmentData,
            SegmentProcessor, SegmentRef,
        };

        /// Offline symbolization processor. Needs the CPU profiler for stack frames.
        #[cfg(feature = "cpu-profiling")]
        pub use dial9_perf_self_profile::SymbolizeProcessor;

        #[cfg(feature = "tokio")]
        pub use dial9_tokio_telemetry::background_task::{BackgroundTaskConfig, PipelineBuilder};

        #[cfg(all(feature = "tokio", feature = "worker-s3"))]
        pub use dial9_tokio_telemetry::background_task::s3;
    }
}

use crate::core::{Encodable, current_handle};

/// Record an event on the calling thread's current handle.
///
///  A no-op when no recorder is installed on the thread (the handle is disabled).
pub fn record_event(event: impl Encodable) {
    current_handle().record_event(event);
}

// Tokio runtime integration.
/// Instrument an async `main` with dial9 telemetry. Replaces `#[tokio::main]`.
#[cfg(feature = "tokio")]
pub use dial9_macro::main;
#[cfg(feature = "tokio")]
pub use dial9_tokio_telemetry::{TracedFuture, TracedRuntime, spawn};

#[cfg(feature = "tokio")]
pub use dial9_tokio_telemetry::telemetry::{
    Dial9TokioHandle, RecorderBuilderTokioExt, TaskDumpConfig, TokioHooks, TracedRuntimeBuilder,
};

/// Offline trace reading and analysis.
#[cfg(all(feature = "tokio", feature = "analysis"))]
pub mod analysis {
    pub use dial9_tokio_telemetry::telemetry::analysis::*;
    pub use dial9_tokio_telemetry::telemetry::analysis_events;
}

#[cfg(feature = "tokio")]
mod env_config;
#[cfg(feature = "tokio")]
pub use env_config::recorder_from_env;

#[cfg(feature = "tokio")]
use crate::core::{BufferMode, SegmentWriter};

/// Build a [`TracedRuntimeBuilder`] from a writer result, or fall back to a disabled
/// (writer-free) one when the writer cannot be created. Works with any writer:
/// [`DiskBuffer`] or [`MemoryBuffer`]. Telemetry stays best-effort: a failed
/// writer logs at `error!` and runs a plain Tokio runtime rather than panicking
/// your service. `configure` is applied on both paths, so your Tokio settings
/// survive the downgrade.
///
/// ```no_run
/// use dial9::DiskBuffer;
/// fn config() -> dial9::TracedRuntimeBuilder {
///     let writer = DiskBuffer::builder()
///         .base_path("/tmp/dial9-traces")
///         .max_total_size(64 * 1024 * 1024)
///         .build();
///     dial9::recorder_or_disabled(writer, |t| { t.worker_threads(4); })
///         .with_task_tracking(true)
/// }
/// ```
///
/// To plug sources that belong on the recorder before `with_tokio`
/// (e.g. `.with_cpu_profiling`), match on the writer result yourself instead.
#[cfg(feature = "tokio")]
pub fn recorder_or_disabled<M, F>(
    writer: std::io::Result<SegmentWriter<M>>,
    configure: F,
) -> TracedRuntimeBuilder<M>
where
    M: BufferMode,
    F: Fn(&mut ::tokio::runtime::Builder) + Send + Sync + 'static,
{
    match writer {
        Ok(writer) => recorder(writer).with_tokio(configure),
        Err(e) => {
            tracing::error!(
                target: "dial9_telemetry",
                "dial9: trace writer setup failed; running without telemetry: {e}"
            );
            TracedRuntimeBuilder::disabled().with_tokio(configure)
        }
    }
}

// One-call `.with_*` source sugar on the recorder builder. Available whenever a
// perf source is compiled in.
#[cfg(any(
    feature = "cpu-profiling",
    feature = "process-resource",
    feature = "linux-socket",
    feature = "memory-profiling"
))]
pub use dial9_perf_self_profile::RecorderPerfExt;

/// CPU sampling and kernel scheduler events.
#[cfg(feature = "cpu-profiling")]
pub mod cpu {
    pub use dial9_perf_self_profile::{
        CpuProfiler, CpuProfilingConfig, CpuSampleSource, SchedEventConfig, SchedProfiler,
    };
}

/// In-process allocation and free tracking.
#[cfg(feature = "memory-profiling")]
pub mod memory {
    pub use dial9_perf_self_profile::memory_profiling::{
        Dial9Allocator, InstallError, MemoryProfiler, MemoryProfilerGuard, MemoryProfilingConfig,
        is_installed,
    };
}

/// Process resource-usage (rusage) source.
#[cfg(feature = "process-resource")]
pub mod process {
    #[cfg(unix)]
    pub use dial9_perf_self_profile::ProcessResourceUsageSource;
    pub use dial9_perf_self_profile::{ProcessResourceUsageConfig, ProcessResourceUsageEvent};
}

/// Socket accept-queue depth source (Linux).
#[cfg(feature = "linux-socket")]
pub mod socket {
    #[cfg(target_os = "linux")]
    pub use dial9_perf_self_profile::SocketAcceptQueuesSource;
    pub use dial9_perf_self_profile::{SocketAcceptQueuesConfig, TcpAcceptQueueEvent};
}

// Tracing-subscriber layer.
#[cfg(feature = "tracing-layer")]
pub use dial9_tokio_telemetry::tracing_layer;
