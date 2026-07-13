//! The Tokio runtime builder.
//!
//! [`TracedRecorder`] is the ergonomic builder, reached via
//! `recorder(w).with_tokio(..)` (the [`RecorderBuilderTokioExt`] trait, which
//! the `dial9` facade re-exports). It configures the Tokio integration plus the
//! segment pipeline and builds a [`TracedRuntime`].
//!
//! [`build_traced`] + [`TokioAttachConfig`] are the lower-level primitive it
//! builds on: they take a fully-assembled core [`RecorderBuilder`] plus Tokio
//! config and wire up the runtime.

use super::builder::{PipelineConfig, assemble_processors};
use super::guard::TelemetryGuard;
use super::runtime_context::{RuntimeContextRegistry, TokioRuntimesSource};
use crate::background_task::{PipelineBuilder, SegmentProcessor};
use crate::primitives::sync::{Arc, Mutex};
use crate::telemetry::TracedRuntime;
use dial9_core::recorder::{RecorderBuilder, RegisterSource};
use dial9_core::source::Source;
use dial9_core::writer::{Disk, WriterMode};
use std::time::Duration;

/// Tokio integration settings for [`build_traced`].
///
/// The default preset assembles `[Symbolize?, Gzip, WriteBack]` (disk) or
/// `[Symbolize?, Gzip]` (memory). Set `custom_processors` for a verbatim
/// pipeline, or `s3_config` for the S3 upload preset.
#[derive(bon::Builder)]
pub struct TokioAttachConfig {
    /// When `false`, build a plain runtime with no telemetry installed.
    #[builder(default = true)]
    enabled: bool,
    /// Install dial9's Tokio runtime hook instrumentation. Default `true`.
    #[builder(default = true)]
    tokio_instrumentation_enabled: bool,
    /// Track task spawn/terminate.
    #[builder(default)]
    task_tracking_enabled: bool,
    /// Human-readable runtime name → segment metadata.
    runtime_name: Option<String>,
    /// A verbatim processor list. Takes precedence over the S3 preset.
    custom_processors: Option<Vec<Box<dyn SegmentProcessor>>>,
    /// S3 upload preset (`[Symbolize?, Gzip, S3]`).
    #[cfg(feature = "worker-s3")]
    s3_config: Option<crate::background_task::s3::S3Config>,
    /// Pre-built S3 client for the upload preset (default credential chain if None).
    #[cfg(feature = "worker-s3")]
    s3_client: Option<aws_sdk_s3::Client>,
    /// Static segment metadata.
    #[builder(default)]
    segment_metadata: Vec<(String, String)>,
    /// Async-backtrace capture config.
    task_dump_config: Option<crate::telemetry::task_dump_config::TaskDumpConfig>,
    /// User-composed Tokio runtime hooks.
    #[builder(default)]
    tokio_hooks: super::TokioHooks,
    /// Bound on the worker drain at shutdown.
    graceful_shutdown_timeout: Option<Duration>,
    /// Sending half of the on-demand dump trigger. Stashed on the session so
    /// [`Dial9Handle::dump_trigger`](crate::telemetry::Dial9Handle::dump_trigger)
    /// can reach it; the matching rx must already feed the core builder.
    dump_trigger: Option<crate::dump::DumpTrigger>,
    /// Post-start memory-profiler install config.
    #[cfg(feature = "memory-profiling")]
    memory_profiling_config:
        Option<dial9_perf_self_profile::memory_profiling::MemoryProfilingConfig>,
}

impl std::fmt::Debug for TokioAttachConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioAttachConfig").finish_non_exhaustive()
    }
}

/// Build a [`TracedRuntime`] from a fully-assembled core builder + Tokio config.
///
/// Most callers want [`TracedRecorder`] (`recorder(w).with_tokio(..)`).
pub fn build_traced<M: WriterMode>(
    core: RecorderBuilder<M>,
    tokio_builder: tokio::runtime::Builder,
    config: TokioAttachConfig,
) -> std::io::Result<TracedRuntime> {
    if !config.enabled {
        let (runtime, guard) = TracedRuntime::build_disabled(tokio_builder)?;
        return Ok(TracedRuntime::from_parts(
            runtime,
            guard,
            config.graceful_shutdown_timeout,
            #[cfg(feature = "memory-profiling")]
            None,
        ));
    }

    // When the writer is namespaced, S3 keys and the embedded segment metadata
    // must use the same boot_id as the on-disk `{boot_id}/` directory, so a
    // local segment and its upload share one identity.
    let namespace_boot_id = core.writer_boot_id().map(str::to_owned);

    #[cfg(feature = "worker-s3")]
    let preset = match (config.custom_processors, config.s3_config) {
        (Some(procs), _) => PipelineConfig::Custom(procs),
        (None, Some(s3)) => {
            let mut uploader =
                crate::background_task::S3PipelineUploader::new(s3, config.s3_client);
            if let Some(boot_id) = &namespace_boot_id {
                uploader.set_boot_id(boot_id.clone());
            }
            PipelineConfig::S3(Box::new(uploader))
        }
        (None, None) => PipelineConfig::Unset,
    };
    #[cfg(not(feature = "worker-s3"))]
    let preset = match config.custom_processors {
        Some(procs) => PipelineConfig::Custom(procs),
        None => PipelineConfig::Unset,
    };

    let mut segment_metadata = config.segment_metadata;
    if let Some(boot_id) = &namespace_boot_id {
        for (key, value) in &mut segment_metadata {
            if key == "boot_id" {
                *value = boot_id.clone();
            }
        }
    }

    // The default / S3 presets symbolize when the CPU profiler source is
    // present (registered via `.with_cpu_profiling` before `.with_tokio`).
    #[cfg(feature = "cpu-profiling")]
    let symbolize = core
        .source_names()
        .any(|n| n == dial9_perf_self_profile::CpuProfiler::SOURCE_NAME);

    let processors = assemble_processors(
        #[cfg(feature = "cpu-profiling")]
        symbolize,
        M::IS_DISK,
        preset,
    );

    let contexts: RuntimeContextRegistry = Arc::new(Mutex::new(Vec::new()));
    let core = core
        .source(TokioRuntimesSource::new(contexts.clone()))
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

    let core_session = core.build();
    let guard = TelemetryGuard::enabled(core_session, contexts.clone(), config.task_dump_config);

    // Stash the dump trigger's tx so `Dial9Handle::dump_trigger` can reach it.
    if let (Some(trigger), Some(shared)) = (config.dump_trigger, guard.shared()) {
        shared.set_dump_trigger(trigger);
    }

    let runtime = if !config.tokio_instrumentation_enabled {
        let mut tokio_builder = tokio_builder;
        tokio_builder.build()?
    } else {
        let handle = guard
            .session_handle()
            .expect("enabled guard has a session handle")
            .clone();
        let shared = guard.shared().expect("enabled guard has shared state");
        super::attach_runtime(
            shared,
            &contexts,
            tokio_builder,
            config.runtime_name,
            &handle,
            config.task_tracking_enabled,
            config.tokio_hooks,
            guard.taskdump_config(),
        )?
    };

    guard.enable();
    #[cfg(feature = "memory-profiling")]
    let memory_profiler_guard =
        crate::telemetry::install_memory_profiler_on_guard(config.memory_profiling_config, &guard);

    Ok(TracedRuntime::from_parts(
        runtime,
        guard,
        config.graceful_shutdown_timeout,
        #[cfg(feature = "memory-profiling")]
        memory_profiler_guard,
    ))
}

// ── Ergonomic builder: `recorder(w).with_tokio(..)` ──────────────────────────

/// Default graceful-shutdown timeout when the caller does not override it.
pub(crate) const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// A configurator closure customizing a [`tokio::runtime::Builder`], stored as
/// `Arc<dyn Fn ...>` so the configurator list is cheaply cloneable.
pub(crate) type TokioConfigurator =
    std::sync::Arc<dyn Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static>;

fn default_tokio_builder() -> tokio::runtime::Builder {
    let mut b = tokio::runtime::Builder::new_multi_thread();
    b.enable_all();
    b
}

pub(crate) fn materialize_tokio_builder(
    configurators: &[TokioConfigurator],
) -> tokio::runtime::Builder {
    let mut b = default_tokio_builder();
    for c in configurators {
        c(&mut b);
    }
    b
}

/// How the segment pipeline should be assembled. Not `W`-typed: the custom
/// processor list and S3 config carry no writer mode.
enum PipelineChoice {
    /// Default preset (`[Symbolize?, Gzip, WriteBack]` disk / `[Symbolize?, Gzip]` memory).
    Default,
    /// A fully user-supplied processor list.
    Custom(Vec<Box<dyn SegmentProcessor>>),
    /// S3 upload preset.
    #[cfg(feature = "worker-s3")]
    S3(crate::background_task::s3::S3Config),
}

mod sealed {
    pub trait Sealed {}
}

impl<W: WriterMode> sealed::Sealed for RecorderBuilder<W> {}

/// Extension trait adding `.with_tokio(..)` to the core [`RecorderBuilder`],
/// transitioning it to a [`TracedRecorder`] that builds a [`TracedRuntime`].
pub trait RecorderBuilderTokioExt<W: WriterMode>: sealed::Sealed + Sized {
    /// Wire this recorder to a Tokio runtime. The closure configures the
    /// runtime's [`tokio::runtime::Builder`] (pre-seeded `new_multi_thread()` +
    /// `enable_all()`); replace `*t` inside to switch flavor.
    ///
    /// Plug sources (`.with_cpu_profiling(..)`, `.source(..)`) on the recorder
    /// *before* this call; they belong to the core builder, and the pipeline
    /// keys off them (e.g. CPU profiling drives symbolization).
    fn with_tokio<F>(self, f: F) -> TracedRecorder<W>
    where
        F: Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static;
}

impl<W: WriterMode> RecorderBuilderTokioExt<W> for RecorderBuilder<W> {
    fn with_tokio<F>(self, f: F) -> TracedRecorder<W>
    where
        F: Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static,
    {
        TracedRecorder::new(self).with_tokio(f)
    }
}

/// Builds a [`TracedRuntime`] from a core [`RecorderBuilder`] plus Tokio
/// integration and a segment pipeline. Reached via
/// [`RecorderBuilderTokioExt::with_tokio`].
#[must_use = "call `.build()` (or pass to `#[dial9::main]`) to start the runtime"]
pub struct TracedRecorder<W: WriterMode = Disk> {
    // `None` only for the disabled, writer-free recorder (see [`disabled`]);
    // every other path owns a fully-built core builder.
    core: Option<RecorderBuilder<W>>,
    tokio_configurators: Vec<TokioConfigurator>,
    enabled: bool,
    tokio_instrumentation_enabled: bool,
    task_tracking_enabled: bool,
    task_dump_config: Option<crate::telemetry::task_dump_config::TaskDumpConfig>,
    runtime_name: Option<String>,
    tokio_hooks: super::TokioHooks,
    segment_metadata: Vec<(String, String)>,
    pipeline: PipelineChoice,
    #[cfg(feature = "worker-s3")]
    s3_client: Option<aws_sdk_s3::Client>,
    graceful_shutdown_timeout: Option<Duration>,
    // On-demand dump wiring: the rx feeds the core builder's trigger source,
    // the tx is stashed post-build so `Dial9Handle::dump_trigger` can find it.
    trigger_rx: Option<crate::dump::DumpRx>,
    dump_trigger: Option<crate::dump::DumpTrigger>,
    #[cfg(feature = "memory-profiling")]
    memory_profiling_config:
        Option<dial9_perf_self_profile::memory_profiling::MemoryProfilingConfig>,
}

impl<W: WriterMode> std::fmt::Debug for TracedRecorder<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracedRecorder").finish_non_exhaustive()
    }
}

impl<W: WriterMode> TracedRecorder<W> {
    /// A disabled recorder: builds a plain Tokio runtime with no telemetry and,
    /// unlike `recorder(w).enabled(false)`, no writer at all. Nothing touches
    /// the disk, so [`recorder_from_env`](crate::telemetry) can take this path
    /// when `DIAL9_ENABLED` is off. Chain `.with_tokio(..)` for Tokio knobs.
    ///
    /// `W` defaults to [`Disk`] and is inferred from the return position, so a
    /// `-> TracedRecorder<Memory>` config can `return TracedRecorder::disabled()`
    /// too.
    pub fn disabled() -> Self {
        Self::from_core(None)
    }
}

impl<W: WriterMode> TracedRecorder<W> {
    fn new(core: RecorderBuilder<W>) -> Self {
        Self::from_core(Some(core))
    }

    fn from_core(core: Option<RecorderBuilder<W>>) -> Self {
        Self {
            core,
            tokio_configurators: Vec::new(),
            enabled: true,
            tokio_instrumentation_enabled: true,
            task_tracking_enabled: false,
            task_dump_config: None,
            runtime_name: None,
            tokio_hooks: super::TokioHooks::default(),
            segment_metadata: Vec::new(),
            pipeline: PipelineChoice::Default,
            #[cfg(feature = "worker-s3")]
            s3_client: None,
            graceful_shutdown_timeout: Some(DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT),
            trigger_rx: None,
            dump_trigger: None,
            #[cfg(feature = "memory-profiling")]
            memory_profiling_config: None,
        }
    }

    /// Queue another configurator for the runtime's [`tokio::runtime::Builder`].
    pub fn with_tokio<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static,
    {
        self.tokio_configurators.push(std::sync::Arc::new(f));
        self
    }

    /// Set to `false` to build a plain runtime with no telemetry installed.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Enable or disable dial9's Tokio runtime instrumentation. Default `true`.
    pub fn with_tokio_instrumentation(mut self, enabled: bool) -> Self {
        self.tokio_instrumentation_enabled = enabled;
        self
    }

    /// Enable or disable task spawn/terminate tracking.
    pub fn with_task_tracking(mut self, enabled: bool) -> Self {
        self.task_tracking_enabled = enabled;
        self
    }

    /// Capture async backtraces at yield points (requires the `taskdump` feature).
    pub fn with_task_dumps(
        mut self,
        config: crate::telemetry::task_dump_config::TaskDumpConfig,
    ) -> Self {
        self.task_dump_config = Some(config);
        self
    }

    /// Human-readable runtime name, recorded into segment metadata.
    pub fn with_runtime_name(mut self, name: impl Into<String>) -> Self {
        self.runtime_name = Some(name.into());
        self
    }

    /// Compose custom Tokio runtime hooks with dial9's. dial9's logic runs
    /// first, then your callbacks fire in registration order. Callable
    /// repeatedly; each call sees the same `TokioHooks`, stacking callbacks.
    pub fn with_tokio_hooks<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut super::TokioHooks),
    {
        f(&mut self.tokio_hooks);
        self
    }

    /// Static metadata embedded in every sealed segment.
    pub fn with_segment_metadata(mut self, entries: Vec<(String, String)>) -> Self {
        self.segment_metadata = entries;
        self
    }

    /// Bound on the worker drain at shutdown.
    pub fn graceful_shutdown(mut self, timeout: Duration) -> Self {
        self.graceful_shutdown_timeout = Some(timeout);
        self
    }

    /// Skip the bounded worker drain at shutdown.
    pub fn disable_graceful_shutdown(mut self) -> Self {
        self.graceful_shutdown_timeout = None;
        self
    }

    /// Flip the background worker into on-demand mode: sealed segments keep
    /// accumulating in the ring, and the pipeline only runs when a dump is
    /// requested. Reach the [`DumpTrigger`](crate::dump::DumpTrigger) from any
    /// runtime thread via [`Dial9Handle::dump_trigger`]. Pass `|_| {}` for the
    /// default, or `|t| t.debounce(window)` to coalesce bursts.
    ///
    /// [`Dial9Handle::dump_trigger`]: crate::telemetry::Dial9Handle::dump_trigger
    pub fn with_dump_trigger<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(&mut crate::dump::DumpTriggerConfig),
    {
        let mut config = crate::dump::DumpTriggerConfig::new();
        configure(&mut config);
        let (mut trigger, rx) = crate::dump::channel();
        if let Some(window) = config.debounce_window() {
            trigger = trigger.with_debounce(window);
        }
        self.trigger_rx = Some(rx);
        self.dump_trigger = Some(trigger);
        self
    }

    /// Configure a fully custom processor pipeline. Disk-only steps like
    /// `write_back()` are out of scope on a memory writer (compile error).
    pub fn with_custom_pipeline<F>(mut self, build: F) -> Self
    where
        F: FnOnce(PipelineBuilder<W>) -> PipelineBuilder<W>,
    {
        let pipeline = build(PipelineBuilder::new());
        self.pipeline = PipelineChoice::Custom(pipeline.into_processors());
        self
    }

    /// Configure the S3 upload preset for sealed segments.
    #[cfg(feature = "worker-s3")]
    pub fn with_s3_uploader(mut self, config: crate::background_task::s3::S3Config) -> Self {
        self.segment_metadata = config
            .as_metadata()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        self.pipeline = PipelineChoice::S3(config);
        self
    }

    /// Use a pre-built S3 client for the upload preset (custom credentials,
    /// endpoint, region, etc.). Call after [`with_s3_uploader`](Self::with_s3_uploader);
    /// without it the AWS default credential chain is used.
    #[cfg(feature = "worker-s3")]
    pub fn with_s3_client(mut self, client: aws_sdk_s3::Client) -> Self {
        self.s3_client = Some(client);
        self
    }

    /// Enable sampled memory allocation profiling (needs the global allocator).
    #[cfg(feature = "memory-profiling")]
    pub fn with_memory_profiling(
        mut self,
        config: dial9_perf_self_profile::memory_profiling::MemoryProfilingConfig,
    ) -> Self {
        self.memory_profiling_config = Some(config);
        self
    }

    /// Build and start the traced runtime.
    pub fn build(self) -> std::io::Result<TracedRuntime> {
        let Some(mut core) = self.core else {
            // No core: the writer-free disabled recorder builds a plain runtime.
            let tokio_builder = materialize_tokio_builder(&self.tokio_configurators);
            let (runtime, guard) = TracedRuntime::build_disabled(tokio_builder)?;
            return Ok(TracedRuntime::from_parts(
                runtime,
                guard,
                self.graceful_shutdown_timeout,
                #[cfg(feature = "memory-profiling")]
                None,
            ));
        };
        if let Some(rx) = self.trigger_rx {
            core = core.trigger(rx);
        }

        let mut custom_processors: Option<Vec<Box<dyn SegmentProcessor>>> = None;
        #[cfg(feature = "worker-s3")]
        let mut s3_config = None;
        match self.pipeline {
            PipelineChoice::Default => {}
            PipelineChoice::Custom(procs) => custom_processors = Some(procs),
            #[cfg(feature = "worker-s3")]
            PipelineChoice::S3(config) => s3_config = Some(config),
        }

        let builder = TokioAttachConfig::builder()
            .enabled(self.enabled)
            .tokio_instrumentation_enabled(self.tokio_instrumentation_enabled)
            .task_tracking_enabled(self.task_tracking_enabled)
            .segment_metadata(self.segment_metadata)
            .tokio_hooks(self.tokio_hooks)
            .maybe_runtime_name(self.runtime_name)
            .maybe_task_dump_config(self.task_dump_config)
            .maybe_graceful_shutdown_timeout(self.graceful_shutdown_timeout)
            .maybe_dump_trigger(self.dump_trigger)
            .maybe_custom_processors(custom_processors);
        #[cfg(feature = "worker-s3")]
        let builder = builder
            .maybe_s3_config(s3_config)
            .maybe_s3_client(self.s3_client);
        #[cfg(feature = "memory-profiling")]
        let builder = builder.maybe_memory_profiling_config(self.memory_profiling_config);

        let tokio_builder = materialize_tokio_builder(&self.tokio_configurators);
        build_traced(core, tokio_builder, builder.build())
    }
}

impl<W: WriterMode> TryFrom<TracedRecorder<W>> for TracedRuntime {
    type Error = std::io::Error;

    fn try_from(recorder: TracedRecorder<W>) -> Result<Self, Self::Error> {
        recorder.build()
    }
}

impl<W: WriterMode> RegisterSource for TracedRecorder<W> {
    fn source(self, source: impl Source + 'static) -> Self {
        Self {
            core: self.core.map(|c| c.source(source)),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The graceful-shutdown timeout set on the recorder flows through the
    // (writer-free) build path into the resulting `TracedRuntime`.
    #[test]
    fn graceful_shutdown_defaults_to_one_second() {
        let traced = TracedRecorder::<Disk>::disabled().build().unwrap();
        assert_eq!(
            traced.graceful_shutdown_timeout,
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn graceful_shutdown_setter_overrides_default() {
        let traced = TracedRecorder::<Disk>::disabled()
            .graceful_shutdown(Duration::from_secs(7))
            .build()
            .unwrap();
        assert_eq!(
            traced.graceful_shutdown_timeout,
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn disable_graceful_shutdown_sets_none() {
        let traced = TracedRecorder::<Disk>::disabled()
            .disable_graceful_shutdown()
            .build()
            .unwrap();
        assert_eq!(traced.graceful_shutdown_timeout, None);
    }
}
