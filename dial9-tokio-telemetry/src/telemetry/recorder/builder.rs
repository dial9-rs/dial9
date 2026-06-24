use crate::primitives::sync::atomic::Ordering;
use crate::primitives::sync::{Arc, Mutex};
#[cfg(feature = "cpu-profiling")]
use crate::rate_limit::rate_limited;
use crate::telemetry::writer::{Disk, SegmentWriter, WriterMode};
use std::path::PathBuf;
use std::time::Duration;

use super::flush_loop::run_flush_loop;
use super::guard::{TelemetryGuard, WorkerHandle};
use super::handle::Dial9Handle;
use super::shared_state::SharedState;
use super::{ControlCommand, attach_runtime};

/// Marker: no trace path has been set yet.
#[derive(Debug)]
#[non_exhaustive]
pub struct NoTracePath;
/// Marker: a trace path has been set.
#[derive(Debug)]
#[non_exhaustive]
pub struct HasTracePath;

/// Marker: no pipeline strategy has been chosen yet. From this state the
/// builder can transition to either S3 (via `with_s3_uploader`) or a custom
/// pipeline (via `with_custom_pipeline`).
#[derive(Debug)]
#[non_exhaustive]
pub struct PipelineUnset;

/// Marker: the S3 preset has been selected. `with_s3_client` is available
/// to bind a pre-built client; `with_custom_pipeline` is not in scope.
#[derive(Debug)]
#[non_exhaustive]
pub struct PipelineS3;

/// Marker: a custom pipeline has been configured. No further pipeline
/// methods are available.
#[derive(Debug)]
#[non_exhaustive]
pub struct PipelineCustom;

pub(super) enum PipelineConfig {
    Unset,
    #[cfg(feature = "worker-s3")]
    S3(Box<crate::background_task::S3PipelineUploader>),
    Custom(Vec<Box<dyn crate::background_task::SegmentProcessor>>),
}

/// Builder for configuring a traced Tokio runtime.
pub struct TracedRuntimeBuilder<P = NoTracePath, M = PipelineUnset, Mode: WriterMode = Disk> {
    pub(super) enabled: bool,
    pub(super) tokio_instrumentation_enabled: bool,
    pub(super) task_tracking_enabled: bool,
    pub(super) task_dump_config: Option<crate::telemetry::task_dump_config::TaskDumpConfig>,
    pub(super) trace_path: Option<PathBuf>,
    pub(super) runtime_name: Option<String>,
    #[cfg(feature = "cpu-profiling")]
    pub(super) cpu_profiling_config: Option<crate::telemetry::cpu_profile::CpuProfilingConfig>,
    #[cfg(feature = "cpu-profiling")]
    pub(super) sched_event_config: Option<crate::telemetry::cpu_profile::SchedEventConfig>,
    pub(super) process_resource_usage_config: Option<crate::telemetry::ProcessResourceUsageConfig>,
    #[cfg(feature = "linux-socket")]
    pub(super) socket_accept_queues_config: Option<crate::telemetry::SocketAcceptQueuesConfig>,
    pub(super) custom_event_sources: Vec<crate::telemetry::custom_events::CustomEventsSource>,
    pub(super) pipeline: PipelineConfig,
    /// Static segment metadata to inject into every rotated segment's
    /// header. The S3 preset populates this from `S3Config::as_metadata`
    /// so traces stay self-describing.
    pub(super) segment_metadata: Vec<(String, String)>,
    pub(super) worker_poll_interval: Option<Duration>,
    pub(super) worker_metrics_sink: Option<metrique_writer::BoxEntrySink>,
    pub(super) trigger_rx: Option<crate::dump::DumpRx>,
    /// Sending half of the trigger channel minted by [`with_dump_trigger`]. Stashed
    /// into [`SharedState`] at build time so it is reachable via
    /// [`Dial9Handle::dump_trigger`](crate::telemetry::Dial9Handle::dump_trigger).
    pub(super) dump_trigger: Option<crate::dump::DumpTrigger>,

    pub(super) tokio_hooks: super::TokioHooks,
    pub(super) _marker: std::marker::PhantomData<(P, M, Mode)>,
}

impl<P, M, Mode: WriterMode> std::fmt::Debug for TracedRuntimeBuilder<P, M, Mode> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracedRuntimeBuilder")
            .finish_non_exhaustive()
    }
}

// Methods available regardless of trace-path or pipeline state.
impl<P, M, Mode: WriterMode> TracedRuntimeBuilder<P, M, Mode> {
    /// Set to `false` to build a plain runtime with no telemetry
    /// installed and a dummy [`TelemetryGuard`]. Defaults to `true`.
    ///
    /// Unlike [`TelemetryGuard::enable`]/[`TelemetryGuard::disable`]
    /// (which toggle recording at runtime), this controls whether
    /// telemetry hooks and threads are installed at all.
    pub fn install(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Enable or disable task spawn/terminate tracking.
    pub fn with_task_tracking(mut self, enabled: bool) -> Self {
        self.task_tracking_enabled = enabled;
        self
    }

    /// Enable or disable dial9's Tokio runtime instrumentation.
    ///
    /// Defaults to `true`. Set this to `false` to build the Tokio runtime
    /// without dial9's Tokio hook instrumentation.
    pub fn with_tokio_instrumentation(mut self, enabled: bool) -> Self {
        self.tokio_instrumentation_enabled = enabled;
        self
    }

    /// Capture async backtraces at yield points for tasks that stay idle
    /// longer than the configured threshold.
    ///
    /// Requires the `taskdump` crate feature to actually record events
    pub fn with_task_dumps(
        mut self,
        config: crate::telemetry::task_dump_config::TaskDumpConfig,
    ) -> Self {
        if cfg!(not(feature = "taskdump")) {
            tracing::warn!(
                "taskdumps enabled but `taskdump` feature was not. No task dumps will be captured."
            )
        }
        self.task_dump_config = Some(config);
        self
    }

    /// Set a human-readable name for this runtime. Used in segment metadata
    /// to map runtime indices to names for the trace viewer.
    pub fn with_runtime_name(mut self, name: impl Into<String>) -> Self {
        self.runtime_name = Some(name.into());
        self
    }

    /// Set static metadata embedded as a `SegmentMetadata` event in every
    /// sealed segment file. Read back during analysis and attached to every
    /// Span.
    ///
    /// [`with_s3_uploader`](Self::with_s3_uploader) injects bucket /
    /// service_name / instance_path / boot_id automatically; call this
    /// method when using [`with_custom_pipeline`](Self::with_custom_pipeline)
    /// (or no pipeline) and you still want those entries — or when you want
    /// to override the preset's defaults.
    ///
    /// Repeated calls **replace** the metadata, matching how
    /// `with_s3_uploader` overwrites on a second call. The last call wins,
    /// so `with_segment_metadata` placed *after* `with_s3_uploader`
    /// overrides the preset's injection.
    pub fn with_segment_metadata(mut self, entries: Vec<(String, String)>) -> Self {
        self.segment_metadata = entries;
        self
    }

    /// Enable CPU profiling with the given configuration (Linux only).
    #[cfg(feature = "cpu-profiling")]
    pub fn with_cpu_profiling(
        mut self,
        config: crate::telemetry::cpu_profile::CpuProfilingConfig,
    ) -> Self {
        self.cpu_profiling_config = Some(config);
        self
    }

    /// Enable per-worker scheduler event capture (Linux only).
    #[cfg(feature = "cpu-profiling")]
    pub fn with_sched_events(
        mut self,
        config: crate::telemetry::cpu_profile::SchedEventConfig,
    ) -> Self {
        self.sched_event_config = Some(config);
        self
    }

    /// Enable process resource usage sampled from `getrusage(RUSAGE_SELF)`.
    pub fn with_process_resource_usage(
        mut self,
        config: crate::telemetry::ProcessResourceUsageConfig,
    ) -> Self {
        self.process_resource_usage_config = Some(config);
        self
    }

    /// Enable TCP listener accept queue snapshots sampled from Linux sock_diag.
    ///
    /// # Performance
    ///
    /// Full scans can be expensive because they walk `/proc/self/fd` to find this
    /// process's listeners. The cost grows with the number of open file descriptors
    /// in this process, including accepted sockets, open files, pipes, and similar
    /// handles.
    ///
    /// To avoid that cost on every sample, this source caches the classification of
    /// TCP listeners visible in the current network namespace. While that listener
    /// set is stable, samples do not need a full file descriptor scan and should be
    /// cheap.
    ///
    /// # Reliability
    ///
    /// Listeners classified as foreign are cached as foreign. If such a listener is
    /// later transferred into this process with `SCM_RIGHTS`, it will not be tracked
    /// while it keeps the same kernel socket identity.
    #[cfg(feature = "linux-socket")]
    pub fn with_socket_accept_queues(
        mut self,
        config: crate::telemetry::SocketAcceptQueuesConfig,
    ) -> Self {
        self.socket_accept_queues_config = Some(config);
        self
    }

    /// Register a custom event callback.
    ///
    /// The callback runs during flush cycles while telemetry is enabled.
    /// Use [`CustomEventsConfig::minimum_interval`](crate::telemetry::CustomEventsConfig::minimum_interval)
    /// to throttle polling-style callbacks. The default interval is
    /// [`Duration::ZERO`], which runs the callback on every flush cycle.
    ///
    /// This method can be called multiple times to configure multiple
    /// callbacks.
    pub fn with_custom_events<F>(
        mut self,
        config: crate::telemetry::CustomEventsConfig,
        callback: F,
    ) -> Self
    where
        F: for<'a> FnMut(&mut crate::telemetry::CustomEventsContext<'a>) + Send + 'static,
    {
        self.custom_event_sources
            .push(crate::telemetry::custom_events::CustomEventsSource::new(
                config, callback,
            ));
        self
    }

    /// Set how often the background worker polls for sealed segments.
    pub fn with_worker_poll_interval(mut self, interval: Duration) -> Self {
        self.worker_poll_interval = Some(interval);
        self
    }

    /// Set a metrics sink for the background worker.
    pub fn with_worker_metrics_sink(mut self, sink: metrique_writer::BoxEntrySink) -> Self {
        self.worker_metrics_sink = Some(sink);
        self
    }

    /// Flip the background worker into on-demand operation.
    ///
    /// Sealed segments keep accumulating in the ring (memory or disk), but
    /// the configured pipeline only runs when a dump is requested. Retrieve
    /// the [`DumpTrigger`](crate::dump::DumpTrigger) from any thread owned by
    /// this runtime via
    /// [`Dial9Handle::dump_trigger`](crate::telemetry::Dial9Handle::dump_trigger);
    /// no need to thread it through your own state. Orthogonal to pipeline
    /// selection: whichever pipeline you would have wired for continuous mode,
    /// you keep. Without this call the worker processes segments continuously,
    /// as today.
    ///
    /// Pass `|_| {}` for the default, or configure coalescing with
    /// [`DumpTriggerConfig::debounce`](crate::dump::DumpTriggerConfig::debounce), e.g.
    /// `with_dump_trigger(|t| t.debounce(window))`.
    ///
    /// If no pipeline is configured the worker never spawns and every dump
    /// request resolves with
    /// [`DumpError::WorkerStopped`](crate::dump::DumpError::WorkerStopped).
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

    /// Configure user-provided callbacks to run alongside dial9's internal
    /// Tokio runtime hooks. dial9's logic always runs first, then the user
    /// callbacks fire in registration order.
    ///
    /// This method can be called multiple times; each call receives a mutable
    /// reference to the same `TokioHooks` instance. Registering the same hook
    /// multiple times (either within one closure or across multiple calls)
    /// stacks the callbacks — all registered callbacks will fire.
    pub fn with_tokio_hooks<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut super::TokioHooks),
    {
        f(&mut self.tokio_hooks);
        self
    }

    /// Attach a new runtime to an existing telemetry session.
    ///
    /// This reuses the `SharedState`, flush thread, writer, and CPU profiler
    /// from the original `TelemetryGuard`. Only the tokio callbacks are
    /// registered on the new builder. The new runtime's workers get a unique
    /// runtime index so their `WorkerId`s don't collide with existing runtimes.
    ///
    /// If [`with_tokio_instrumentation(false)`](Self::with_tokio_instrumentation)
    /// was set, this builds a plain runtime instead.
    pub fn build_and_attach_to_telemetry(
        self,
        mut builder: tokio::runtime::Builder,
        guard: &TelemetryGuard,
    ) -> std::io::Result<tokio::runtime::Runtime> {
        let (Some(shared), Some(contexts), Some(control_tx)) =
            (guard.shared(), guard.contexts(), guard.control_tx())
        else {
            // Disabled guard: produce a plain tokio runtime with no
            // telemetry hooks so attaching still works gracefully.
            return builder.build();
        };
        let custom_event_sources = self.custom_event_sources;
        #[cfg(feature = "linux-socket")]
        let socket_accept_queues_config = self.socket_accept_queues_config;

        if !self.tokio_instrumentation_enabled {
            let runtime = builder.build()?;
            #[cfg(feature = "linux-socket")]
            if let Some(config) = socket_accept_queues_config {
                push_socket_accept_queues_source(shared, config);
            }
            for source in custom_event_sources {
                shared.push_source(Box::new(source));
            }
            return Ok(runtime);
        }

        let runtime = attach_runtime(
            shared,
            contexts,
            builder,
            self.runtime_name,
            control_tx,
            self.task_tracking_enabled,
            self.tokio_hooks,
        )?;
        #[cfg(feature = "linux-socket")]
        if let Some(config) = socket_accept_queues_config {
            push_socket_accept_queues_source(shared, config);
        }
        for source in custom_event_sources {
            shared.push_source(Box::new(source));
        }
        Ok(runtime)
    }

    pub(crate) fn into_state<Q, N, NewMode: WriterMode>(
        self,
    ) -> TracedRuntimeBuilder<Q, N, NewMode> {
        TracedRuntimeBuilder {
            enabled: self.enabled,
            tokio_instrumentation_enabled: self.tokio_instrumentation_enabled,
            task_tracking_enabled: self.task_tracking_enabled,
            task_dump_config: self.task_dump_config,
            trace_path: self.trace_path,
            runtime_name: self.runtime_name,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: self.cpu_profiling_config,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: self.sched_event_config,
            process_resource_usage_config: self.process_resource_usage_config,
            #[cfg(feature = "linux-socket")]
            socket_accept_queues_config: self.socket_accept_queues_config,
            custom_event_sources: self.custom_event_sources,
            pipeline: self.pipeline,
            segment_metadata: self.segment_metadata,
            worker_poll_interval: self.worker_poll_interval,
            worker_metrics_sink: self.worker_metrics_sink,
            trigger_rx: self.trigger_rx,
            dump_trigger: self.dump_trigger,
            tokio_hooks: self.tokio_hooks,
            _marker: std::marker::PhantomData,
        }
    }
}

// Pipeline-strategy entry points: only available before a strategy has been
// chosen, so the user picks S3 OR a custom pipeline, not both. These are the
// only place where `Mode` gets injected into the typestate — before this
// point the builder carries the default `Mode = Disk` placeholder.
impl<P> TracedRuntimeBuilder<P, PipelineUnset> {
    /// Configure the S3 upload preset for sealed trace segments.
    ///
    /// The resulting pipeline is `[Gzip, S3]` (with `[Symbolize, ...]`
    /// prepended when CPU profiling is enabled). After this call, only
    /// [`with_s3_client`](TracedRuntimeBuilder::with_s3_client) and a
    /// repeated [`with_s3_uploader`](TracedRuntimeBuilder::with_s3_uploader)
    /// override are available — `with_custom_pipeline` is no longer in scope.
    ///
    /// `Mode` is a fresh writer-mode parameter unified with the writer at
    /// build time: the S3 preset works against either disk or memory writers.
    #[cfg(feature = "worker-s3")]
    pub fn with_s3_uploader<Mode: WriterMode>(
        mut self,
        config: crate::background_task::s3::S3Config,
    ) -> TracedRuntimeBuilder<P, PipelineS3, Mode> {
        self.segment_metadata = config
            .as_metadata()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        self.pipeline = PipelineConfig::S3(Box::new(
            crate::background_task::S3PipelineUploader::new(config, None),
        ));
        self.into_state()
    }

    /// Configure a fully custom processor pipeline. The closure receives a
    /// [`PipelineBuilder`](crate::background_task::PipelineBuilder); chain
    /// methods like `.gzip()`, `.write_back()`, `.s3(cfg)` for built-ins
    /// and `.pipe(processor)` for user-supplied processors.
    ///
    /// Mutually exclusive with [`with_s3_uploader`](Self::with_s3_uploader).
    ///
    /// This is the "full control" path: the resulting pipeline is exactly
    /// what the closure builds, with nothing prepended or appended. In
    /// particular, unlike the S3 preset, this path does **not**:
    /// - auto-populate writer-side segment metadata — call
    ///   [`with_segment_metadata`](Self::with_segment_metadata) if you want
    ///   identity entries (service, host, etc.) embedded in trace files.
    /// - auto-prepend the `Symbolize` step when CPU profiling is enabled.
    ///   Chain
    ///   [`.symbolize()`](crate::background_task::PipelineBuilder::symbolize)
    ///   first if you want symbolized stack frames.
    ///
    /// `Mode` is pinned by disk-only methods inside the closure (e.g.
    /// `.write_back()` forces `Disk`) or inferred from the writer at build.
    /// Pairing `.write_back()` with `InMemoryWriter` is a configuration error.
    ///
    /// ```compile_fail
    /// use dial9_tokio_telemetry::telemetry::{InMemoryWriter, TracedRuntime};
    /// let writer = InMemoryWriter::new(4 * 1024 * 1024).unwrap();
    /// let mut tk = tokio::runtime::Builder::new_current_thread();
    /// tk.enable_all();
    /// let _ = TracedRuntime::builder()
    ///     .with_custom_pipeline(|p| p.write_back())
    ///     .build(tk, writer);
    /// ```
    pub fn with_custom_pipeline<F, Mode>(
        mut self,
        build: F,
    ) -> TracedRuntimeBuilder<P, PipelineCustom, Mode>
    where
        Mode: WriterMode,
        F: FnOnce(
            crate::background_task::PipelineBuilder<Mode>,
        ) -> crate::background_task::PipelineBuilder<Mode>,
    {
        let pipeline = build(crate::background_task::PipelineBuilder::new());
        self.pipeline = PipelineConfig::Custom(pipeline.into_processors());
        self.into_state()
    }
}

// S3 mode — once the S3 preset is chosen, only S3-specific tweaks remain.
#[cfg(feature = "worker-s3")]
impl<P, Mode: WriterMode> TracedRuntimeBuilder<P, PipelineS3, Mode> {
    /// Provide a pre-built S3 client (for custom credentials or endpoints).
    /// Replaces any client previously bound to the configured S3 uploader.
    pub fn with_s3_client(mut self, client: aws_sdk_s3::Client) -> Self {
        if let PipelineConfig::S3(ref mut uploader) = self.pipeline {
            uploader.set_client(client);
        }
        self
    }

    /// Replace the configured S3 uploader. A client previously bound via
    /// [`with_s3_client`](Self::with_s3_client) is carried over to the new
    /// uploader so that call order between the two is irrelevant.
    pub fn with_s3_uploader(mut self, config: crate::background_task::s3::S3Config) -> Self {
        self.segment_metadata = config
            .as_metadata()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let carried = match &mut self.pipeline {
            PipelineConfig::S3(uploader) => uploader.take_client(),
            _ => None,
        };
        self.pipeline = PipelineConfig::S3(Box::new(
            crate::background_task::S3PipelineUploader::new(config, carried),
        ));
        self
    }
}

impl<M, Mode: WriterMode> TracedRuntimeBuilder<NoTracePath, M, Mode> {
    /// Set the trace output path. This transitions the builder to
    /// `HasTracePath`, enabling `build()` and `build_and_start()`.
    pub fn with_trace_path(
        mut self,
        path: impl Into<PathBuf>,
    ) -> TracedRuntimeBuilder<HasTracePath, M, Mode> {
        self.trace_path = Some(path.into());
        self.into_state()
    }
}

/// Build methods for the no-pipeline state. The writer drives `Mode`: pass a
/// [`DiskWriter`] or [`InMemoryWriter`](crate::telemetry::InMemoryWriter) and
/// the mode is inferred. Generic over the trace-path state `P` and the
/// builder's current mode `BMode` (a no-pipeline builder never pins a mode, so
/// the writer's `Mode` re-types it freely). Mode-bound pipeline states have
/// their own `build`, where the writer mode must match the pinned `Mode`.
impl<P, BMode: WriterMode> TracedRuntimeBuilder<P, PipelineUnset, BMode> {
    /// Build the traced runtime. Recording starts disabled. `Mode` is inferred
    /// from `writer`. The background worker spawns only when a pipeline is set,
    /// so a plain no-pipeline build never starts one.
    pub fn build<Mode: WriterMode>(
        self,
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.into_state::<HasTracePath, PipelineUnset, Mode>()
            .build_inner(builder, writer)
    }

    /// Build the traced runtime and immediately enable recording. `Mode` is
    /// inferred from `writer`.
    pub fn build_and_start<Mode: WriterMode>(
        self,
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let (runtime, guard) = self.build(builder, writer)?;
        guard.enable();
        Ok((runtime, guard))
    }
}

impl<M, Mode: WriterMode> TracedRuntimeBuilder<HasTracePath, M, Mode> {
    /// Set the trace output path (no-op, already set).
    pub fn with_trace_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.trace_path = Some(path.into());
        self
    }

    fn build_inner(
        self,
        mut builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        if !self.enabled {
            return TracedRuntime::build_disabled(builder);
        }

        let custom_event_sources = self.custom_event_sources;
        let dump_trigger = self.dump_trigger;

        let processors = assemble_processors(
            #[cfg(feature = "cpu-profiling")]
            self.cpu_profiling_config.is_some(),
            Mode::IS_DISK,
            self.pipeline,
        );

        let core_builder = TelemetryCore::builder()
            .writer(writer)
            .maybe_trace_path(self.trace_path)
            .maybe_task_dump_config(self.task_dump_config)
            .maybe_process_resource_usage(self.process_resource_usage_config);

        #[cfg(feature = "linux-socket")]
        let core_builder =
            core_builder.maybe_socket_accept_queues(self.socket_accept_queues_config);

        let core_builder = core_builder
            .maybe_worker_poll_interval(self.worker_poll_interval)
            .maybe_worker_metrics_sink(self.worker_metrics_sink)
            .maybe_trigger(self.trigger_rx)
            .processors(processors)
            .segment_metadata(self.segment_metadata);

        #[cfg(feature = "cpu-profiling")]
        let core_builder = core_builder
            .maybe_cpu_profiling(self.cpu_profiling_config)
            .maybe_sched_events(self.sched_event_config);

        let guard = core_builder.build()?;

        if let Some(shared) = guard.shared() {
            for source in custom_event_sources {
                shared.push_source(Box::new(source));
            }
            if let Some(trigger) = dump_trigger {
                shared.set_dump_trigger(trigger);
            }
        }

        if !self.tokio_instrumentation_enabled {
            let runtime = builder.build()?;
            return Ok((runtime, guard));
        }

        let control_tx = guard
            .control_tx()
            .expect("TelemetryCore::builder().build() always returns an enabled guard")
            .clone();
        let shared = guard
            .shared()
            .expect("TelemetryCore::builder().build() always returns an enabled guard");
        let contexts = guard
            .contexts()
            .expect("TelemetryCore::builder().build() always returns an enabled guard");
        let runtime = attach_runtime(
            shared,
            contexts,
            builder,
            self.runtime_name,
            &control_tx,
            self.task_tracking_enabled,
            self.tokio_hooks,
        )?;
        Ok((runtime, guard))
    }
}

/// Build methods for a custom-pipeline runtime. The pipeline pins `Mode`, so
/// the writer must match it (a `Disk` pipeline cannot take a `Memory` writer).
/// Generic over the trace-path state `P` (the worker still only spawns once a
/// path is set; a no-path build just skips it).
impl<P, Mode: WriterMode> TracedRuntimeBuilder<P, PipelineCustom, Mode> {
    /// Build the traced runtime. Recording starts disabled.
    pub fn build(
        self,
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.into_state::<HasTracePath, PipelineCustom, Mode>()
            .build_inner(builder, writer)
    }

    /// Build the traced runtime and immediately enable recording.
    pub fn build_and_start(
        self,
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let (runtime, guard) = self.build(builder, writer)?;
        guard.enable();
        Ok((runtime, guard))
    }
}

/// Build methods for an S3-pipeline runtime. The pipeline pins `Mode`, so the
/// writer must match it. Generic over the trace-path state `P`.
#[cfg(feature = "worker-s3")]
impl<P, Mode: WriterMode> TracedRuntimeBuilder<P, PipelineS3, Mode> {
    /// Build the traced runtime. Recording starts disabled.
    pub fn build(
        self,
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.into_state::<HasTracePath, PipelineS3, Mode>()
            .build_inner(builder, writer)
    }

    /// Build the traced runtime and immediately enable recording.
    pub fn build_and_start(
        self,
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        let (runtime, guard) = self.build(builder, writer)?;
        guard.enable();
        Ok((runtime, guard))
    }
}

/// Crate-internal: re-unifies `build_and_start` across the pipeline-marker
/// states so the `#[main]` macro / [`crate::Dial9Config`] path can stay generic
/// over the marker `N`. The public build methods are split per state (to infer
/// `Mode` only on the safe no-pipeline state); this trait lets the erased macro
/// path call a single method regardless of marker.
///
/// Public-but-hidden: it appears in the `where` bounds of the public
/// `Dial9Config` builder methods (`with_runtime`/`build`), so it must be at
/// least as visible as them to satisfy `private_interfaces`.
#[doc(hidden)]
pub trait BuildAndStartRuntime<Mode: WriterMode> {
    fn build_and_start_runtime(
        self,
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)>;
}

impl<BMode: WriterMode, Mode: WriterMode> BuildAndStartRuntime<Mode>
    for TracedRuntimeBuilder<HasTracePath, PipelineUnset, BMode>
{
    fn build_and_start_runtime(
        self,
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.build_and_start(builder, writer)
    }
}

impl<Mode: WriterMode> BuildAndStartRuntime<Mode>
    for TracedRuntimeBuilder<HasTracePath, PipelineCustom, Mode>
{
    fn build_and_start_runtime(
        self,
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.build_and_start(builder, writer)
    }
}

#[cfg(feature = "worker-s3")]
impl<Mode: WriterMode> BuildAndStartRuntime<Mode>
    for TracedRuntimeBuilder<HasTracePath, PipelineS3, Mode>
{
    fn build_and_start_runtime(
        self,
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        self.build_and_start(builder, writer)
    }
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

#[cfg(feature = "linux-socket")]
fn push_socket_accept_queues_source(
    shared: &Arc<SharedState>,
    config: crate::telemetry::SocketAcceptQueuesConfig,
) {
    #[cfg(target_os = "linux")]
    shared.push_source(Box::new(
        crate::telemetry::socket_accept_queues::SocketAcceptQueuesSource::new(config),
    ));

    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        tracing::warn!("socket accept queues enabled but sock_diag is only available on Linux");
    }
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
        /// Path for trace output. Enables the background worker when any
        /// segment processors are configured.
        #[builder(into)]
        trace_path: Option<PathBuf>,
        /// Capture async backtraces at yield points. Requires the `taskdump`
        /// crate feature to actually record events.
        task_dump_config: Option<crate::telemetry::task_dump_config::TaskDumpConfig>,
        /// Enable CPU profiling (Linux only).
        #[cfg(feature = "cpu-profiling")]
        cpu_profiling: Option<crate::telemetry::cpu_profile::CpuProfilingConfig>,
        /// Enable scheduler event capture (Linux only).
        #[cfg(feature = "cpu-profiling")]
        sched_events: Option<crate::telemetry::cpu_profile::SchedEventConfig>,
        /// Enable process resource usage sampled from `getrusage(RUSAGE_SELF)`.
        process_resource_usage: Option<crate::telemetry::ProcessResourceUsageConfig>,
        /// Enable TCP listener accept queue snapshots sampled from Linux sock_diag.
        #[cfg(feature = "linux-socket")]
        socket_accept_queues: Option<crate::telemetry::SocketAcceptQueuesConfig>,
        /// How often the background worker polls for sealed segments.
        worker_poll_interval: Option<Duration>,
        /// Metrics sink for the flush/worker threads.
        worker_metrics_sink: Option<metrique_writer::BoxEntrySink>,
        /// Trigger receiver flipping the background worker into on-demand
        /// operation; see [`crate::dump`]. `None` keeps continuous mode.
        #[builder(setters(vis = "pub(crate)"))]
        trigger: Option<crate::dump::DumpRx>,
    ) -> std::io::Result<TelemetryGuard> {
        let start_mono_ns = crate::telemetry::events::clock_monotonic_ns();
        let rng_seed = task_dump_config.as_ref().and_then(|cfg| cfg.rng_seed());
        let shared = Arc::new(SharedState::new(start_mono_ns, rng_seed));
        if let Some(cfg) = task_dump_config.as_ref() {
            shared.task_dumps_enabled.store(true, Ordering::Relaxed);
            shared
                .task_dump_idle_threshold_ns
                .store(cfg.idle_threshold().as_nanos() as u64, Ordering::Relaxed);
        }

        // Determine the pipeline strategy from the builder fields, then
        // delegate to `assemble_processors` — the single source of truth for
        // which processors are used in each configuration.
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

        #[allow(unused_mut)]
        let mut writer = writer;
        let writer_fs = writer.fs_handle();

        let processors = assemble_processors(
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling.is_some(),
            M::IS_DISK,
            pipeline,
        );

        if !segment_metadata.is_empty() {
            writer.update_segment_metadata(segment_metadata);
        }

        let contexts: super::runtime_context::RuntimeContextRegistry =
            Arc::new(Mutex::new(Vec::new()));
        shared.push_source(Box::new(super::runtime_context::TokioRuntimesSource::new(
            contexts.clone(),
        )));

        if let Some(config) = process_resource_usage {
            #[cfg(unix)]
            shared.push_source(Box::new(
                crate::telemetry::process_resource_usage::ProcessResourceUsageSource::new(config),
            ));
            #[cfg(not(unix))]
            {
                let _ = config;
                tracing::warn!(
                    "process resource usage enabled but getrusage is not available on this platform"
                );
            }
        }

        #[cfg(feature = "linux-socket")]
        {
            if let Some(config) = socket_accept_queues {
                push_socket_accept_queues_source(&shared, config);
            }
        }

        #[cfg(feature = "cpu-profiling")]
        {
            if let Some(ref config) = cpu_profiling {
                match crate::telemetry::cpu_profile::CpuProfiler::start(config.clone()) {
                    Ok(sampler) => shared.push_source(Box::new(sampler)),
                    Err(e) => rate_limited!(Duration::from_secs(60), {
                        tracing::warn!("failed to start CPU profiler: {e}");
                    }),
                }
            }
            if let Some(sched_cfg) = sched_events {
                match crate::telemetry::cpu_profile::SchedProfiler::new(sched_cfg) {
                    Ok(sched) => shared.push_source(Box::new(sched)),
                    Err(e) => rate_limited!(Duration::from_secs(60), {
                        tracing::warn!("failed to start scheduler event profiler: {e}");
                    }),
                }
            }
        }

        // Channel for Dial9Handle/Guard → flush thread communication.
        let (control_tx, control_rx) =
            crate::primitives::sync::mpsc::sync_channel::<ControlCommand>(1);

        let flush_metrics_sink = worker_metrics_sink
            .clone()
            .unwrap_or_else(metrique_writer::sink::DevNullSink::boxed);

        let flush_thread = {
            let shared = shared.clone();
            crate::primitives::thread::spawn_named("dial9-flush", move || {
                #[cfg(target_os = "linux")]
                // SAFETY: nice() is a simple syscall with no memory safety
                // implications. Increasing the nice value (lowering priority)
                // is always permitted for unprivileged processes.
                unsafe {
                    let _ = libc::nice(10);
                }

                #[cfg(feature = "cpu-profiling")]
                let _ = dial9_perf_self_profile::register_current_thread();
                run_flush_loop(control_rx, &shared, &flush_metrics_sink, writer);
                #[cfg(feature = "cpu-profiling")]
                dial9_perf_self_profile::unregister_current_thread();
            })
        };

        // Spawn the background worker when we have a filesystem backend
        // (disk or memory via `writer_fs`) and at least one processor.
        let worker_config = if processors.is_empty() {
            None
        } else if let Some(fs) = writer_fs {
            let poll_interval =
                worker_poll_interval.unwrap_or(crate::background_task::DEFAULT_POLL_INTERVAL);
            let metrics_sink =
                worker_metrics_sink.unwrap_or_else(metrique_writer::sink::DevNullSink::boxed);

            let config = if let Some(tp) = trace_path {
                crate::background_task::BackgroundTaskConfig::builder()
                    .trace_path(tp)
                    .poll_interval(poll_interval)
                    .processors(processors)
                    .metrics_sink(metrics_sink)
                    .maybe_trigger(trigger)
                    .build()
            } else {
                crate::background_task::BackgroundTaskConfig::builder()
                    .poll_interval(poll_interval)
                    .processors(processors)
                    .metrics_sink(metrics_sink)
                    .maybe_trigger(trigger)
                    .build()
            };
            Some((config, fs))
        } else {
            None
        };

        #[allow(unused_mut)]
        let mut worker = None;
        if let Some((config, fs)) = worker_config {
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            let wt = crate::primitives::thread::spawn_named("dial9-worker", move || {
                #[cfg(feature = "cpu-profiling")]
                let _ = dial9_perf_self_profile::register_current_thread();
                crate::background_task::run_background_task(config, shutdown_rx, fs);
                #[cfg(feature = "cpu-profiling")]
                dial9_perf_self_profile::unregister_current_thread();
            });
            worker = Some(WorkerHandle {
                shutdown: Some(shutdown_tx),
                thread: Some(wt),
            });
        }

        Ok(TelemetryGuard::enabled(
            Dial9Handle::enabled(shared, control_tx),
            Some(flush_thread),
            worker,
            contexts,
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
/// Construct one of two ways:
///
/// - **High-level**: from a [`crate::Dial9Config`] via [`TracedRuntime::new`]
///   (panicking, used by the `#[dial9_tokio_telemetry::main]` macro) or
///   [`TracedRuntime::try_new`] (fallible).
/// - **Low-level**: via [`TracedRuntime::builder`] →
///   [`build_and_start`](TracedRuntimeBuilder::build_and_start) for direct
///   control over the raw [`tokio::runtime::Builder`] and the
///   [`DiskWriter`](crate::telemetry::DiskWriter) /
///   [`InMemoryWriter`](crate::telemetry::InMemoryWriter). This is the path
///   used by example code, benchmarks, and integration tests.
#[derive(Debug)]
pub struct TracedRuntime {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) guard: TelemetryGuard,
    #[cfg(feature = "memory-profiling")]
    pub(crate) memory_profiler_guard: Option<crate::memory_profiling::MemoryProfilerGuard>,
    /// Graceful-shutdown timeout carried from the [`crate::Dial9Config`].
    /// Consumed by [`graceful_shutdown`](TracedRuntime::graceful_shutdown)
    /// (used by the `#[dial9_tokio_telemetry::main]` macro). `None` skips the
    /// implicit drain.
    pub(crate) graceful_shutdown_timeout: Option<Duration>,
}

impl TracedRuntime {
    /// Create a new [`TracedRuntimeBuilder`].
    pub fn builder() -> TracedRuntimeBuilder<NoTracePath, PipelineUnset> {
        TracedRuntimeBuilder {
            enabled: true,
            tokio_instrumentation_enabled: true,
            task_tracking_enabled: false,
            task_dump_config: None,
            trace_path: None,
            runtime_name: None,
            #[cfg(feature = "cpu-profiling")]
            cpu_profiling_config: None,
            #[cfg(feature = "cpu-profiling")]
            sched_event_config: None,
            process_resource_usage_config: None,
            #[cfg(feature = "linux-socket")]
            socket_accept_queues_config: None,
            custom_event_sources: Vec::new(),
            pipeline: PipelineConfig::Unset,
            segment_metadata: Vec::new(),
            worker_poll_interval: None,
            worker_metrics_sink: None,
            trigger_rx: None,
            dump_trigger: None,
            tokio_hooks: super::TokioHooks::default(),
            _marker: std::marker::PhantomData,
        }
    }

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

    /// Build the traced runtime. Recording starts disabled. `Mode` is inferred
    /// from the writer.
    pub fn build<Mode: WriterMode>(
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        Self::builder().build(builder, writer)
    }

    /// Build the traced runtime and immediately enable recording. `Mode` is
    /// inferred from the writer.
    pub fn build_and_start<Mode: WriterMode>(
        builder: tokio::runtime::Builder,
        writer: SegmentWriter<Mode>,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        Self::builder().build_and_start(builder, writer)
    }
}

// ---------------------------------------------------------------------------
// High-level construction: TracedRuntime::new / try_new from Dial9Config
// ---------------------------------------------------------------------------

/// Errors produced while constructing a [`TracedRuntime`] from a
/// [`crate::Dial9Config`].
///
/// Writer-transport I/O has already been validated by the config builder's
/// strict `build`, so the only remaining failure modes here come from the
/// tokio runtime builder and the telemetry background worker startup.
#[derive(Debug)]
#[non_exhaustive]
pub enum TelemetryRuntimeError {
    /// Failure from [`tokio::runtime::Builder::build`].
    TokioRuntimeBuilder(std::io::Error),
    /// Failure from telemetry core setup (traced runtime + background worker).
    TelemetryCore(std::io::Error),
}

impl std::fmt::Display for TelemetryRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelemetryRuntimeError::TokioRuntimeBuilder(e) => {
                write!(f, "tokio runtime builder: {e}")
            }
            TelemetryRuntimeError::TelemetryCore(e) => write!(f, "telemetry core: {e}"),
        }
    }
}

impl std::error::Error for TelemetryRuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TelemetryRuntimeError::TokioRuntimeBuilder(e)
            | TelemetryRuntimeError::TelemetryCore(e) => Some(e),
        }
    }
}

/// Drive a [`crate::current_config::Inner`] to a tokio runtime + guard.
///
/// `Inner::Enabled` carries a `runtime_builder` that already owns its
/// writer, so this only materializes the tokio builder and starts it.
/// `Inner::Disabled` produces a plain tokio runtime paired with a disabled [`TelemetryGuard`].
fn try_assemble_dial9_config(
    inner: crate::current_config::Inner,
) -> Result<(tokio::runtime::Runtime, TelemetryGuard), TelemetryRuntimeError> {
    use crate::current_config::{Inner, materialize_tokio_builder};

    match inner {
        Inner::Enabled {
            tokio_configurators,
            runtime_builder,
        } => {
            let tokio_builder = materialize_tokio_builder(&tokio_configurators);
            let (runtime, guard) =
                runtime_builder(tokio_builder).map_err(TelemetryRuntimeError::TelemetryCore)?;
            Ok((runtime, guard))
        }
        Inner::Disabled {
            tokio_configurators,
        } => {
            let runtime = materialize_tokio_builder(&tokio_configurators)
                .build()
                .map_err(TelemetryRuntimeError::TokioRuntimeBuilder)?;
            Ok((runtime, TelemetryGuard::disabled()))
        }
    }
}

#[cfg(feature = "memory-profiling")]
fn install_memory_profiler(
    config: Option<crate::memory_profiling::MemoryProfilingConfig>,
    guard: &TelemetryGuard,
) -> Option<crate::memory_profiling::MemoryProfilerGuard> {
    let config = config?;
    if !guard.is_enabled() {
        return None;
    }

    match crate::memory_profiling::MemoryProfiler::from_config(config).install(guard.handle()) {
        Ok(memory_guard) => Some(memory_guard),
        Err(e) => {
            tracing::warn!(target: "dial9_telemetry", "failed to install memory profiler: {e}");
            None
        }
    }
}

impl TracedRuntime {
    /// Build a [`TracedRuntime`] from a config, panicking with the
    /// underlying error on failure. Used by the
    /// `#[dial9_tokio_telemetry::main]` macro.
    ///
    /// Reach for this directly when the macro doesn't fit — e.g. when an
    /// application owns multiple tokio runtimes, when you need to control
    /// runtime lifetime explicitly, or when you want to drive
    /// [`TelemetryGuard::graceful_shutdown`] before the runtime drops.
    ///
    /// Generic over any input that converts into a [`TracedRuntime`]: in
    /// practice that means the fluent [`crate::Dial9Config`] (returned by
    /// [`Dial9Config::builder`](crate::Dial9Config::builder)). The generic
    /// shape is what keeps the macro source-compatible across input types.
    ///
    /// # Panics
    ///
    /// Panics if the underlying conversion fails — i.e. if the tokio
    /// runtime cannot be built or the telemetry background worker fails
    /// to start. When constructing from the fluent
    /// [`crate::Dial9Config`], writer-transport I/O has already been
    /// validated by the config builder's strict `build`, so the only
    /// remaining failure modes are tokio-builder and telemetry-core
    /// startup I/O.
    ///
    /// For fallible construction, use [`try_new`](Self::try_new).
    ///
    /// ```no_run
    /// use dial9_tokio_telemetry::{Dial9Config, TracedRuntime};
    /// let cfg = Dial9Config::builder()
    ///     .on_disk_buffer("trace.bin")
    ///     .max_file_size(64 * 1024 * 1024)
    ///     .max_total_size(1024 * 1024 * 1024)
    ///     .build()?;
    /// let rt = TracedRuntime::new(cfg);
    /// rt.block_on(async { /* ... */ });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
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
    /// Returns the conversion error directly: when constructing from
    /// [`crate::Dial9Config`] that's a [`TelemetryRuntimeError`]. Use this
    /// when you want to handle runtime construction failure rather than
    /// panic.
    ///
    /// ```no_run
    /// use dial9_tokio_telemetry::{Dial9Config, TracedRuntime};
    /// let cfg = Dial9Config::builder()
    ///     .on_disk_buffer("trace.bin")
    ///     .max_file_size(64 * 1024 * 1024)
    ///     .max_total_size(1024 * 1024 * 1024)
    ///     .build()?;
    /// let rt = TracedRuntime::try_new(cfg)?;
    /// rt.block_on(async { /* ... */ });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
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
    /// This is what `#[dial9_tokio_telemetry::main]` calls after the body
    /// completes. It:
    ///
    /// 1. drops the tokio runtime so worker threads exit and flush their
    ///    thread-local telemetry buffers, then
    /// 2. if a graceful-shutdown timeout was configured on the
    ///    [`crate::Dial9Config`] (the default is 1s; `None` when disabled via
    ///    [`disable_graceful_shutdown`](crate::DiskConfigBuilder::disable_graceful_shutdown)),
    ///    calls [`TelemetryGuard::graceful_shutdown`] with that timeout to
    ///    drain the background worker.
    ///
    /// Typically paired with [`block_on`](Self::block_on):
    ///
    /// ```no_run
    /// # use dial9_tokio_telemetry::{Dial9Config, TracedRuntime};
    /// # let cfg = Dial9Config::builder().on_disk_buffer("trace.bin").max_total_size(1 << 20).build()?;
    /// let rt = TracedRuntime::new(cfg);
    /// let out = rt.block_on(async { /* ... */ });
    /// rt.graceful_shutdown();
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// The drain is best-effort: any error returned by
    /// [`TelemetryGuard::graceful_shutdown`] is logged at `error!` and
    /// otherwise ignored. When you need the deadline at a call site, the
    /// configured value is available via the original [`crate::Dial9Config`];
    /// the low-level [`TelemetryGuard::graceful_shutdown`] also takes an
    /// explicit timeout.
    pub fn graceful_shutdown(self) {
        let Self {
            runtime,
            guard,
            #[cfg(feature = "memory-profiling")]
                memory_profiler_guard: _memory_profiler_guard,
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
}

impl TryFrom<crate::Dial9Config> for TracedRuntime {
    type Error = TelemetryRuntimeError;

    fn try_from(config: crate::Dial9Config) -> Result<Self, Self::Error> {
        #[cfg(feature = "memory-profiling")]
        let memory_profiling_config = config.memory_profiling_config;
        let graceful_shutdown_timeout = config.graceful_shutdown_timeout;
        let (runtime, guard) = try_assemble_dial9_config(config.inner)?;
        #[cfg(feature = "memory-profiling")]
        let memory_profiler_guard = install_memory_profiler(memory_profiling_config, &guard);
        Ok(Self {
            runtime,
            guard,
            #[cfg(feature = "memory-profiling")]
            memory_profiler_guard,
            graceful_shutdown_timeout,
        })
    }
}
