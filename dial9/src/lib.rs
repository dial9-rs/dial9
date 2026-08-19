#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = include_str!("../README.md")]

// Core recording API
pub use dial9_core::buffer::{Disk, DiskBuffer, Memory, MemoryBuffer};
pub use dial9_core::handle::Dial9Handle;
pub use dial9_core::recorder::{
    RecorderBuilder, RecorderSourceExt, recorder, recorder_disabled, recorder_or_disabled,
};
pub use dial9_core::recording::Recorder;

/// The trace format: define events with `#[derive(TraceEvent)]`, encode your own
/// types with [`TraceField`](format::TraceField), read a trace back with
/// [`Decoder`](format::Decoder).
pub mod format {
    pub use dial9_trace_format::decoder::{self, Decoder};
    pub use dial9_trace_format::types::{self, EventEncoder, FieldType, InternedString};
    pub use dial9_trace_format::{TraceEvent, TraceField};

    /// Error from [`RawEvent::deserialize`](decoder::RawEvent::deserialize).
    #[cfg(feature = "deserialize")]
    pub use dial9_trace_format::DeserError;
}

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
    pub use dial9_core::schema_extensions;
    pub use dial9_core::source::{self, FlushContext, Source};
    pub use dial9_core::thread::{ThreadTrackingGuard, current_tid};

    // Background pipeline (segment worker, on-demand dumps).
    #[cfg(feature = "pipeline")]
    pub use dial9_core::{dump, worker};

    /// Segment pipeline: processors, offline symbolization, and (with `tokio`)
    /// the pipeline builder and worker config.
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
    }
}

/// Upload sealed segments to S3.
#[cfg(all(feature = "tokio", feature = "worker-s3"))]
pub mod s3 {
    pub use dial9_destinations_s3::{
        InstanceIdentity, KeyContext, S3Config, S3ConfigBuilder, S3KeyFn, S3PipelineUploader,
    };
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
pub use dial9_tokio_telemetry::{TracedFuture, block_on, spawn, spawn_in};

#[cfg(all(feature = "tokio", feature = "worker-s3"))]
pub use dial9_tokio_telemetry::telemetry::RecorderS3ClientExt;
#[cfg(feature = "tokio")]
pub use dial9_tokio_telemetry::telemetry::{
    AttachedRuntime, Dial9HandleTokioExt, Dial9TokioHandle, JoinSetExt, RecorderPipelineExt,
    TaskDumpConfig, TokioAttachOptions, TokioHooks,
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
pub use env_config::{recorder_from_env, recorder_from_env_with};

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
        CpuProfiler, CpuProfilingConfig, CpuSampleSource, EventSource, SchedEventConfig,
        SchedProfiler,
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

// Metrique unit-of-work entry sink.
#[cfg(feature = "metrique-sink")]
pub use dial9_metrique as metrique_sink;

// The metrique field flags at the crate root, so `#[metrics(...)]`
// attributes read as `flags(dial9::Interned)` / `flags(dial9::Skip)` /
// `flags(dial9::SpanName)`
// without imports.
#[cfg(feature = "metrique-sink")]
pub use dial9_metrique::{Interned, Skip, SpanName};
