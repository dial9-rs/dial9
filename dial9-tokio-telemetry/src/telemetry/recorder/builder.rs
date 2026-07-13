use crate::primitives::sync::{Arc, Mutex};
use crate::telemetry::writer::{SegmentWriter, WriterMode};
use std::time::Duration;

use super::guard::TelemetryGuard;

pub(super) enum PipelineConfig {
    Unset,
    #[cfg(feature = "worker-s3")]
    S3(Box<crate::background_task::S3PipelineUploader>),
    Custom(Vec<Box<dyn crate::background_task::SegmentProcessor>>),
}

/// Build the final processor pipeline.
///
/// `Symbolize` is auto-prepended for the built-in presets (`Unset`, `S3`)
/// when CPU profiling is enabled. The `Custom` path is "full control" — the
/// user's processor list is passed through verbatim, and they're expected to
/// chain [`PipelineBuilder::symbolize`](crate::background_task::PipelineBuilder::symbolize)
/// themselves if they want symbolization.
///
/// Behaviour matrix:
///
/// | strategy | CPU profiling on (disk)        | CPU profiling on (memory) | CPU profiling off |
/// |----------|--------------------------------|---------------------------|-------------------|
/// | Unset    | `[Symbolize, Gzip, WriteBack]` | `[Symbolize, Gzip]`       | (worker skipped)  |
/// | S3       | `[Symbolize, Gzip, S3]`        | `[Symbolize, Gzip, S3]`   | `[Gzip, S3]`      |
/// | Custom   | `[...user]`                    | `[...user]`               | `[...user]`       |
pub(super) fn assemble_processors(
    #[cfg(feature = "cpu-profiling")] cpu_profiling_enabled: bool,
    is_disk: bool,
    pipeline: PipelineConfig,
) -> Vec<Box<dyn crate::background_task::SegmentProcessor>> {
    #[cfg(not(feature = "cpu-profiling"))]
    let cpu_profiling_enabled = false;

    if matches!(pipeline, PipelineConfig::Unset) && !cpu_profiling_enabled {
        return Vec::new();
    }

    let mut processors: Vec<Box<dyn crate::background_task::SegmentProcessor>> = Vec::new();
    match pipeline {
        PipelineConfig::Unset => {
            #[cfg(feature = "cpu-profiling")]
            if cpu_profiling_enabled {
                processors.push(Box::new(crate::background_task::SymbolizeProcessor::new()));
            }
            processors.push(Box::new(crate::background_task::GzipCompressor));
            if is_disk {
                processors.push(Box::new(
                    crate::background_task::WriteBackProcessor::default(),
                ));
            }
        }
        #[cfg(feature = "worker-s3")]
        PipelineConfig::S3(uploader) => {
            #[cfg(feature = "cpu-profiling")]
            if cpu_profiling_enabled {
                processors.push(Box::new(crate::background_task::SymbolizeProcessor::new()));
            }
            processors.push(Box::new(crate::background_task::GzipCompressor));
            processors.push(uploader);
        }
        PipelineConfig::Custom(user) => {
            processors.extend(user);
        }
    }
    processors
}

/// Entry point for creating a telemetry session decoupled from any tokio runtime.
///
/// Use [`TelemetryCore::builder()`] to configure the session, then call
/// [`TelemetryGuard::trace_runtime`] to attach one or more runtimes.
///
/// ```rust,no_run
/// # use dial9_tokio_telemetry::telemetry::{DiskWriter, TelemetryCore};
/// # fn main() -> std::io::Result<()> {
/// let writer = DiskWriter::single_file("/tmp/trace.bin")?;
/// let guard = TelemetryCore::builder()
///     .writer(writer)
///     .build()?;
/// guard.enable();
///
/// let mut builder = tokio::runtime::Builder::new_multi_thread();
/// builder.worker_threads(4).enable_all();
/// let (runtime, handle) = guard.trace_runtime("main").build(builder)?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct TelemetryCore;

#[bon::bon]
impl TelemetryCore {
    /// Build a telemetry session. Recording starts disabled; call
    /// [`TelemetryGuard::enable`] to begin recording.
    #[builder(state_mod = telemetry_core_builder)]
    pub fn new<M: WriterMode>(
        /// The pipeline of [`SegmentProcessor`](crate::background_task::SegmentProcessor)s
        /// to run on each sealed segment. When empty the background worker
        /// is not spawned.
        #[builder(field)]
        processors: Vec<Box<dyn crate::background_task::SegmentProcessor>>,
        /// Static segment metadata injected into every rotated segment's
        /// header. Empty by default; the S3 preset populates it from the
        /// configured `S3Config` so traces stay self-describing.
        #[builder(field)]
        segment_metadata: Vec<(String, String)>,
        /// S3 upload configuration.
        #[cfg(feature = "worker-s3")]
        #[builder(field)]
        s3_config: Option<crate::background_task::s3::S3Config>,
        /// Pre-built S3 client.
        #[cfg(feature = "worker-s3")]
        #[builder(field)]
        s3_client: Option<aws_sdk_s3::Client>,
        /// The trace writer ([`DiskWriter`] or [`InMemoryWriter`](crate::telemetry::InMemoryWriter)).
        writer: SegmentWriter<M>,
        /// Capture async backtraces at yield points. Requires the `taskdump`
        /// crate feature to actually record events.
        task_dump_config: Option<crate::telemetry::task_dump_config::TaskDumpConfig>,
        /// How often the background worker polls for sealed segments.
        worker_poll_interval: Option<Duration>,
        /// Metrics sink for the flush/worker threads.
        worker_metrics_sink: Option<metrique::writer::BoxEntrySink>,
        /// Trigger receiver flipping the background worker into on-demand
        /// operation; see [`crate::dump`]. `None` keeps continuous mode.
        #[builder(setters(vis = "pub(crate)"))]
        trigger: Option<crate::dump::DumpRx>,
    ) -> std::io::Result<TelemetryGuard> {
        // Determine the pipeline strategy from the builder fields, then
        // delegate to `assemble_processors` — the single source of truth for
        // which processors are used in each configuration. The `Custom`
        // pass-through means a list pre-assembled by the caller is not
        // re-symbolized.
        #[allow(unused_mut)]
        let mut segment_metadata = segment_metadata;

        #[allow(unused_variables)]
        let pipeline = if !processors.is_empty() {
            PipelineConfig::Custom(processors)
        } else {
            #[cfg(feature = "worker-s3")]
            if let Some(config) = s3_config {
                if segment_metadata.is_empty() {
                    segment_metadata = config
                        .as_metadata()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                }
                PipelineConfig::S3(Box::new(crate::background_task::S3PipelineUploader::new(
                    config, s3_client,
                )))
            } else {
                PipelineConfig::Unset
            }
            #[cfg(not(feature = "worker-s3"))]
            PipelineConfig::Unset
        };

        // Multi-runtime sessions plug perf `Source`s through the recorder
        // builder before attach, so this path installs none itself: no CPU
        // profiler means nothing to symbolize.
        let processors = assemble_processors(
            #[cfg(feature = "cpu-profiling")]
            false,
            M::IS_DISK,
            pipeline,
        );

        // The Tokio runtime-context source carries the runtime name into segment
        // metadata, `contexts` is retained so the guard can attach runtimes.
        let contexts: super::runtime_context::RuntimeContextRegistry =
            Arc::new(Mutex::new(Vec::new()));

        //  Setup a recorder and feed it the Tokio-specific source, the assembled pipeline,
        // and the perf thread-registration hook.
        #[allow(unused_mut)]
        let mut builder = dial9_core::recorder::recorder(writer)
            .source(super::runtime_context::TokioRuntimesSource::new(
                contexts.clone(),
            ))
            .segment_metadata(segment_metadata)
            .processors(processors)
            .on_recording_thread_start(|| {
                #[cfg(feature = "cpu-profiling")]
                let _ = dial9_perf_self_profile::register_current_thread();
                move || {
                    #[cfg(feature = "cpu-profiling")]
                    dial9_perf_self_profile::unregister_current_thread();
                }
            });
        if let Some(sink) = worker_metrics_sink {
            builder = builder.metrics_sink(sink);
        }
        if let Some(interval) = worker_poll_interval {
            builder = builder.worker_poll_interval(interval);
        }
        if let Some(trigger) = trigger {
            builder = builder.trigger(trigger);
        }

        let core_session = builder.build();

        Ok(TelemetryGuard::enabled(
            core_session,
            contexts,
            task_dump_config,
        ))
    }
}

// Custom methods on the generated builder.
impl<M: WriterMode, S: telemetry_core_builder::State> TelemetryCoreBuilder<M, S> {
    /// Configure S3 upload for sealed trace segments.
    #[cfg(feature = "worker-s3")]
    pub fn s3_config(mut self, config: crate::background_task::s3::S3Config) -> Self {
        self.s3_config = Some(config);
        self
    }

    /// Provide a pre-built S3 client (for custom credentials or endpoints).
    #[cfg(feature = "worker-s3")]
    pub fn s3_client(mut self, client: aws_sdk_s3::Client) -> Self {
        self.s3_client = Some(client);
        self
    }

    /// Set the processor pipeline directly.
    pub fn processors(
        mut self,
        processors: Vec<Box<dyn crate::background_task::SegmentProcessor>>,
    ) -> Self {
        self.processors = processors;
        self
    }

    /// Set static segment metadata.
    pub fn segment_metadata(mut self, entries: Vec<(String, String)>) -> Self {
        self.segment_metadata = entries;
        self
    }
}

/// A tokio runtime paired with its (optional) dial9 telemetry guard.
///
/// The guard, when present, must outlive the runtime so traces are flushed
/// on drop — keeping both inside one struct enforces that ordering at the
/// type level (fields drop top-to-bottom, so `runtime` drops before `guard`).
///
/// Construct it from a [`TracedRecorder`](super::TracedRecorder)
/// (`recorder(w).with_tokio(..)`) via [`TracedRuntime::new`] (panicking, used by
/// the `#[dial9::main]` macro) or [`TracedRuntime::try_new`] (fallible). For a
/// multi-runtime setup, build a [`TelemetryCore`] session and attach runtimes
/// with [`TelemetryGuard::trace_runtime`] instead.
#[derive(Debug)]
pub struct TracedRuntime {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) guard: TelemetryGuard,
    /// Graceful-shutdown timeout carried from the [`TracedRecorder`](super::TracedRecorder).
    /// Consumed by [`graceful_shutdown`](TracedRuntime::graceful_shutdown)
    /// (used by the `#[dial9::main]` macro). `None` skips the
    /// implicit drain.
    pub(crate) graceful_shutdown_timeout: Option<Duration>,
}

impl TracedRuntime {
    /// Build a plain runtime with no telemetry installed.
    ///
    /// The returned [`TelemetryGuard`] is in its disabled mode — see
    /// [`TelemetryGuard::is_enabled`].
    pub fn build_disabled(
        mut builder: tokio::runtime::Builder,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let runtime = builder.build()?;
        Ok((runtime, TelemetryGuard::disabled()))
    }
}

// ---------------------------------------------------------------------------
// High-level construction: TracedRuntime::new / try_new
// ---------------------------------------------------------------------------

impl TracedRuntime {
    /// Build a [`TracedRuntime`] from a config, panicking with the
    /// underlying error on failure. Used by the
    /// `#[dial9::main]` macro.
    ///
    /// Reach for this directly when the macro doesn't fit — e.g. when an
    /// application owns multiple tokio runtimes, when you need to control
    /// runtime lifetime explicitly, or when you want to drive
    /// [`TelemetryGuard::graceful_shutdown`] before the runtime drops.
    ///
    /// Generic over any input that converts into a [`TracedRuntime`]: in
    /// practice that means a [`TracedRecorder`](super::TracedRecorder)
    /// (from `recorder(w).with_tokio(..)`). The generic shape is what keeps
    /// the macro source-compatible across input types.
    ///
    /// # Panics
    ///
    /// Panics if the underlying conversion fails — i.e. if the tokio
    /// runtime cannot be built or the telemetry background worker fails
    /// to start.
    ///
    /// For fallible construction, use [`try_new`](Self::try_new).
    pub fn new<C>(config: C) -> Self
    where
        C: TryInto<TracedRuntime>,
        <C as TryInto<TracedRuntime>>::Error: std::fmt::Display,
    {
        config
            .try_into()
            .unwrap_or_else(|e| panic!("failed to initialize runtime: {e}"))
    }

    /// Fallible counterpart to [`new`](Self::new).
    ///
    /// Returns the conversion error directly. Use this when you want to handle
    /// runtime construction failure rather than panic.
    pub fn try_new<C>(config: C) -> Result<Self, <C as TryInto<TracedRuntime>>::Error>
    where
        C: TryInto<TracedRuntime>,
    {
        config.try_into()
    }

    /// Borrow the underlying tokio runtime.
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }

    /// Borrow the telemetry guard.
    ///
    /// The guard is always present, regardless of whether telemetry was
    /// installed. Use [`TelemetryGuard::is_enabled`] to distinguish a
    /// live telemetry session from an inert (disabled) guard.
    pub fn guard(&self) -> &TelemetryGuard {
        &self.guard
    }

    /// Run `fut` to completion on the runtime.
    ///
    /// The future is always spawned through a
    /// [`Dial9TokioHandle`](super::handle::Dial9TokioHandle) bound to this
    /// runtime. On an enabled guard this records poll and wake events; on
    /// a disabled guard the handle's `spawn` falls through to plain
    /// [`tokio::spawn`].
    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = self.guard.tokio_handle(self.runtime.handle());
        self.runtime.block_on(async move {
            match handle.spawn(fut).await {
                Ok(output) => output,
                Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
                Err(_) => unreachable!("task cannot be cancelled inside block_on"),
            }
        })
    }

    /// Drop the runtime and perform the configured graceful shutdown.
    ///
    /// This is what `#[dial9::main]` calls after the body
    /// completes. It:
    ///
    /// 1. drops the tokio runtime so worker threads exit and flush their
    ///    thread-local telemetry buffers, then
    /// 2. if a graceful-shutdown timeout was configured on the
    ///    [`TracedRecorder`](super::TracedRecorder) (the default is 1s; `None`
    ///    when disabled via
    ///    [`TracedRecorder::disable_graceful_shutdown`](super::TracedRecorder::disable_graceful_shutdown)),
    ///    calls [`TelemetryGuard::graceful_shutdown`] with that timeout to
    ///    drain the background worker.
    ///
    /// Typically paired with [`block_on`](Self::block_on): build the runtime
    /// from a [`TracedRecorder`](super::TracedRecorder), run the body with
    /// `block_on`, then call `graceful_shutdown`.
    ///
    /// The drain is best-effort: any error returned by
    /// [`TelemetryGuard::graceful_shutdown`] is logged at `error!` and
    /// otherwise ignored. When you need the deadline at a call site, the
    /// low-level [`TelemetryGuard::graceful_shutdown`] also takes an
    /// explicit timeout.
    pub fn graceful_shutdown(self) {
        let Self {
            runtime,
            guard,
            graceful_shutdown_timeout,
        } = self;
        // Drop the runtime first so Tokio worker threads exit and flush their
        // thread-local buffers into the collector before the guard drains the
        // background worker.
        drop(runtime);
        if let Some(timeout) = graceful_shutdown_timeout
            && let Err(e) = guard.graceful_shutdown(timeout)
        {
            tracing::error!(target: "dial9_telemetry", error = %e, "dial9 graceful shutdown failed");
        }
    }

    /// Assemble a [`TracedRuntime`] from already-built parts.
    ///
    /// Prefer [`new`](Self::new) / [`try_new`](Self::try_new) for the
    /// usual high-level construction.
    pub fn from_parts(
        runtime: tokio::runtime::Runtime,
        guard: TelemetryGuard,
        graceful_shutdown_timeout: Option<Duration>,
    ) -> Self {
        Self {
            runtime,
            guard,
            graceful_shutdown_timeout,
        }
    }

    /// Decompose into owned parts. The inverse of [`from_parts`](Self::from_parts).
    ///
    /// Reach for this when you need to own the runtime and guard separately:
    /// sequence shutdown yourself, keep the guard past the runtime, or drive
    /// [`TelemetryGuard::graceful_shutdown`] to get the drain result. Most
    /// callers want [`graceful_shutdown`](Self::graceful_shutdown).
    pub fn into_parts(self) -> (tokio::runtime::Runtime, TelemetryGuard, Option<Duration>) {
        (self.runtime, self.guard, self.graceful_shutdown_timeout)
    }
}
