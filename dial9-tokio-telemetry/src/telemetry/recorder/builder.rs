use std::time::Duration;

use crate::primitives::sync::{Arc, Mutex};
use crate::telemetry::task_dump_config::TaskDumpConfig;
use dial9_core::handle::Dial9Handle;
use dial9_core::recording::Recorder;

use super::SharedState;
use super::guard::RuntimeAttach;
use super::handle::Dial9TokioHandle;
use super::runtime_context::RuntimeContextRegistry;

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

/// A Tokio runtime plus its dial9 recorder.
///
/// Returned by `recorder(w).with_tokio(..).build()`. It owns a primary tokio
/// runtime and the recorder, and hosts any number of additional
/// runtimes that share the same trace.
///
/// - Drive the primary runtime: [`block_on`](Self::block_on),
///   [`runtime`](Self::runtime), [`handle`](Self::handle).
/// - Attach more runtimes to the same trace: [`trace_runtime`](Self::trace_runtime).
/// - End it: [`shutdown`](Self::shutdown).
///
/// When telemetry is disabled (opted out, or a lenient config downgraded after a
/// build failure) the runtime is a plain tokio runtime and the telemetry methods
/// are inert. Use [`is_enabled`](Self::is_enabled) to tell.
pub struct TracedRuntime {
    // `runtime` is dropped first (Tokio workers exit and flush their
    // thread-locals) before `recorder` seals the final segment.
    runtime: tokio::runtime::Runtime,
    /// The recorder; `None` when telemetry is disabled.
    recorder: Option<Recorder>,
    /// Registry of runtimes attached to this recorder (empty when disabled).
    contexts: RuntimeContextRegistry,
    /// Task-dump settings installed on attached runtimes.
    taskdump_config: Option<TaskDumpConfig>,
    /// Drain bound used by [`graceful_shutdown`](Self::graceful_shutdown). `None`
    /// skips the drain. Set via
    /// [`TracedRuntimeBuilder::graceful_shutdown`](super::TracedRuntimeBuilder::graceful_shutdown).
    graceful_shutdown_timeout: Option<Duration>,
}

impl std::fmt::Debug for TracedRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracedRuntime")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl TracedRuntime {
    /// Assemble an enabled recorder from its parts. Called by
    /// [`build_traced`](super::build_traced).
    pub(crate) fn enabled(
        runtime: tokio::runtime::Runtime,
        recorder: Recorder,
        contexts: RuntimeContextRegistry,
        taskdump_config: Option<TaskDumpConfig>,
        graceful_shutdown_timeout: Option<Duration>,
    ) -> Self {
        Self {
            runtime,
            recorder: Some(recorder),
            contexts,
            taskdump_config,
            graceful_shutdown_timeout,
        }
    }

    /// Build a plain runtime with no telemetry installed (the disabled recorder).
    pub(crate) fn build_disabled(
        mut builder: tokio::runtime::Builder,
        graceful_shutdown_timeout: Option<Duration>,
    ) -> std::io::Result<Self> {
        let runtime = builder.build()?;
        Ok(Self {
            runtime,
            recorder: None,
            contexts: Arc::new(Mutex::new(Vec::new())),
            taskdump_config: None,
            graceful_shutdown_timeout,
        })
    }

    /// Build a [`TracedRuntime`] from a config, panicking with the underlying
    /// error on failure. Used by the `#[dial9::main]` macro.
    ///
    /// Generic over any input that converts into a [`TracedRuntime`] — in
    /// practice a [`TracedRuntimeBuilder`](super::TracedRuntimeBuilder) (from
    /// `recorder(w).with_tokio(..)`). The generic shape keeps the macro
    /// source-compatible across input types.
    ///
    /// # Panics
    ///
    /// Panics if the tokio runtime cannot be built or the telemetry background
    /// worker fails to start. For fallible construction, use
    /// [`try_new`](Self::try_new).
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
    pub fn try_new<C>(config: C) -> Result<Self, <C as TryInto<TracedRuntime>>::Error>
    where
        C: TryInto<TracedRuntime>,
    {
        config.try_into()
    }

    /// Borrow the primary tokio runtime.
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }

    /// A [`Dial9TokioHandle`] for spawning instrumented tasks on the primary
    /// runtime. Use `handle.spawn(..)` instead of `tokio::spawn` for wake-event
    /// tracking. Inert when telemetry is disabled.
    pub fn handle(&self) -> Dial9TokioHandle {
        self.tokio_handle(self.runtime.handle())
    }

    /// A [`Dial9TokioHandle`] for spawning instrumented tasks on `runtime`.
    /// Inert when telemetry is disabled.
    pub fn tokio_handle(&self, runtime: &tokio::runtime::Handle) -> Dial9TokioHandle {
        Dial9TokioHandle::for_runtime(runtime.clone(), super::traced_handle(&self.record_handle()))
    }

    /// The recording [`Dial9Handle`] (for `record_event` and enable/disable).
    /// Inert when telemetry is disabled.
    pub fn record_handle(&self) -> Dial9Handle {
        self.recorder
            .as_ref()
            .map_or_else(Dial9Handle::disabled, |s| s.handle().clone())
    }

    /// Whether this recorder is recording (vs an inert, disabled recorder).
    pub fn is_enabled(&self) -> bool {
        self.recorder.is_some()
    }

    /// Begin recording. No-op when disabled.
    pub fn enable(&self) {
        if let Some(s) = &self.recorder {
            s.enable();
        }
    }

    /// Stop recording. No-op when disabled.
    pub fn disable(&self) {
        if let Some(s) = &self.recorder {
            s.disable();
        }
    }

    /// Monotonic recording start time in nanoseconds, if recording.
    pub fn start_time(&self) -> Option<u64> {
        self.recorder.as_ref().and_then(|s| s.start_time())
    }

    /// The underlying runtime-agnostic recorder. `None` when disabled.
    pub fn recorder(&self) -> Option<&Recorder> {
        self.recorder.as_ref()
    }

    /// The configured task-dump settings, if any.
    pub fn taskdump_config(&self) -> Option<TaskDumpConfig> {
        self.taskdump_config
    }

    /// Attach another named tokio runtime to this recorder, so several runtimes
    /// share one trace. You build and own the returned runtime; drop it before
    /// [`shutdown`](Self::shutdown) so its workers flush.
    ///
    /// ```no_run
    /// # use dial9_tokio_telemetry::telemetry::{DiskBuffer, RecorderBuilderTokioExt, recorder};
    /// let traced = recorder(DiskBuffer::single_file("/tmp/trace.bin")?)
    ///     .with_tokio(|t| { t.worker_threads(4); })
    ///     .build()?;
    /// let (io_rt, io_handle) = traced
    ///     .trace_runtime("io")
    ///     .build(tokio::runtime::Builder::new_multi_thread())?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn trace_runtime(&self, name: impl Into<String>) -> RuntimeAttach<'_> {
        RuntimeAttach::new(self, name.into())
    }

    /// Run `fut` to completion on the primary runtime.
    ///
    /// The future is spawned through a [`Dial9TokioHandle`] bound to the
    /// primary runtime, so on an enabled recorder this records poll and wake
    /// events; on a disabled one it falls through to plain `tokio::spawn`.
    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = self.handle();
        self.runtime.block_on(async move {
            match handle.spawn(fut).await {
                Ok(output) => output,
                Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
                Err(_) => unreachable!("task cannot be cancelled inside block_on"),
            }
        })
    }

    /// Drop the primary runtime, then drain the background worker within
    /// `timeout`.
    ///
    /// **Call this after any runtimes you attached with
    /// [`trace_runtime`](Self::trace_runtime) have been dropped**, so their
    /// worker threads have flushed. Consumes the runtime, a no-op when disabled.
    /// Best-effort: drain errors are logged at `error!`. To skip the drain
    /// entirely, just drop the runtime.
    pub fn graceful_shutdown(self, timeout: Duration) {
        let Self {
            runtime, recorder, ..
        } = self;
        // Drop the runtime first so Tokio workers exit and flush before the
        // recorder seals the final segment.
        drop(runtime);
        if let Some(s) = recorder
            && let Err(e) = s.graceful_shutdown(timeout)
        {
            tracing::error!(target: "dial9_telemetry", error = %e, "dial9 graceful shutdown failed");
        }
    }

    /// The worker-drain deadline configured on the builder,
    /// or `None` when [`disable_graceful_shutdown`](super::TracedRuntimeBuilder::disable_graceful_shutdown)
    /// was set.
    pub fn graceful_shutdown_timeout(&self) -> Option<Duration> {
        self.graceful_shutdown_timeout
    }

    // ── Internal accessors for RuntimeAttach ──────────────────────

    pub(crate) fn shared(&self) -> Option<&Arc<SharedState>> {
        self.recorder.as_ref().and_then(|s| s.shared())
    }

    pub(crate) fn contexts_registry(&self) -> Option<&RuntimeContextRegistry> {
        self.recorder.as_ref().map(|_| &self.contexts)
    }

    pub(crate) fn session_handle(&self) -> Option<&Dial9Handle> {
        self.recorder.as_ref().map(|s| s.handle())
    }
}
