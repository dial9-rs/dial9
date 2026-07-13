//! The main crate for dial9 telemetry.
//!
//! Most applications want the `tokio` feature: `#[dial9::main]`, `TracedRuntime`,
//! `spawn`, and the `recorder(w).with_tokio(..)` builder. Library authors who
//! only need to record events into a trace can use the always-available core API
//! ([`recorder`](fn@recorder), [`Dial9Handle`], [`record_event`], [`Source`])
//! without pulling in Tokio.
//!
//! Profiling (`cpu-profiling`, `memory-profiling`), the tracing-subscriber
//! layer, and S3 upload are behind feature gates. The viewer CLI ships in the
//! `dial9` binary (the `cli` feature, on by default).

// Core recording API
pub use dial9_core::buffer::{self, Encodable, ThreadLocalEncoder};
pub use dial9_core::clock::{self, clock_monotonic_ns};
pub use dial9_core::handle::{self, Dial9Handle, clear_tl_handle, current_handle, set_tl_handle};
pub use dial9_core::recorder::{self, RecorderBuilder, RegisterSource, recorder};
pub use dial9_core::session::{self, CoreSession};
pub use dial9_core::source::{self, FlushContext, Source};
pub use dial9_core::writer::{
    self, Disk, DiskWriter, InMemoryWriter, Memory, SegmentWriter, WriterMode,
};

/// Record an event on the calling thread's current handle.
///
///  A no-op when no session is installed on the thread (the handle is disabled).
pub fn record_event(event: impl Encodable) {
    current_handle().record_event(event);
}

// Background pipeline (segment worker, processors, on-demand dumps).
#[cfg(feature = "pipeline")]
pub use dial9_core::{dump, pipeline, worker};

// Tokio runtime integration.
/// Instrument an async `main` with dial9 telemetry. Replaces `#[tokio::main]`.
#[cfg(feature = "tokio")]
pub use dial9_macro::main;
#[cfg(feature = "tokio")]
pub use dial9_tokio_telemetry::{TracedFuture, TracedRuntime, background_task, spawn, telemetry};

#[cfg(feature = "tokio")]
pub use dial9_tokio_telemetry::telemetry::{RecorderBuilderTokioExt, TracedRecorder};

#[cfg(feature = "tokio")]
mod config;
#[cfg(feature = "tokio")]
pub use config::recorder_from_env;

/// Build a [`TracedRecorder`] from a writer result, or fall back to a disabled
/// (writer-free) one when the writer cannot be created. Works with any writer:
/// [`DiskWriter`] or [`InMemoryWriter`]. Telemetry stays best-effort: a failed
/// writer logs at `error!` and runs a plain Tokio runtime rather than panicking
/// your service. `configure` is applied on both paths, so your Tokio settings
/// survive the downgrade.
///
/// ```no_run
/// use dial9::DiskWriter;
/// fn config() -> dial9::TracedRecorder {
///     let writer = DiskWriter::builder()
///         .base_path("/tmp/trace.bin")
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
) -> TracedRecorder<M>
where
    M: WriterMode,
    F: Fn(&mut ::tokio::runtime::Builder) + Send + Sync + 'static,
{
    match writer {
        Ok(writer) => recorder(writer).with_tokio(configure),
        Err(e) => {
            tracing::error!(
                target: "dial9_telemetry",
                "dial9: trace writer setup failed; running without telemetry: {e}"
            );
            TracedRecorder::disabled().with_tokio(configure)
        }
    }
}

// CPU and scheduler profiling.
#[cfg(feature = "cpu-profiling")]
pub use dial9_perf_self_profile::{
    CpuProfiler, CpuProfilingConfig, CpuSampleSource, SchedEventConfig, SchedProfiler,
};

// One-call `.with_*` source sugar on the recorder builder. Available whenever a
// perf source is compiled in.
#[cfg(any(
    feature = "cpu-profiling",
    feature = "process-resource",
    feature = "linux-socket"
))]
pub use dial9_perf_self_profile::RecorderPerfExt;

// Offline symbolization processor for the segment pipeline. Needs both the CPU
// profiler (to produce stack frames) and the pipeline (to run the processor).
#[cfg(all(feature = "cpu-profiling", feature = "pipeline"))]
pub use dial9_perf_self_profile::SymbolizeProcessor;

// Memory profiling.
#[cfg(feature = "memory-profiling")]
pub use dial9_perf_self_profile::{AllocEvent, FreeEvent, memory_profiling};

// Process resource usage source.
#[cfg(all(feature = "process-resource", unix))]
pub use dial9_perf_self_profile::ProcessResourceUsageSource;
#[cfg(feature = "process-resource")]
pub use dial9_perf_self_profile::{ProcessResourceUsageConfig, ProcessResourceUsageEvent};

// Socket accept-queue source.
#[cfg(all(feature = "linux-socket", target_os = "linux"))]
pub use dial9_perf_self_profile::SocketAcceptQueuesSource;
#[cfg(feature = "linux-socket")]
pub use dial9_perf_self_profile::{SocketAcceptQueuesConfig, TcpAcceptQueueEvent};

// Tracing-subscriber layer.
#[cfg(feature = "tracing-layer")]
pub use dial9_tokio_telemetry::tracing_layer;

/// Brings the extension traits into scope so the `.with_*` source sugar is
/// callable: `use dial9::prelude::*;`.
pub mod prelude {
    #[cfg(feature = "tokio")]
    pub use crate::RecorderBuilderTokioExt;
    #[cfg(any(
        feature = "cpu-profiling",
        feature = "process-resource",
        feature = "linux-socket"
    ))]
    pub use crate::RecorderPerfExt;
    pub use crate::RegisterSource;
}
