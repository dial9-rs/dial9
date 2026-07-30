//! Build Tokio runtimes instrumented against a [`Recorder`].
//!
//! [`RecorderTokioExt::attach_tokio_runtime`] registers dial9 hooks on a
//! `tokio::runtime::Builder`, builds the runtime, and returns both. Call it once
//! per runtime, all attached runtimes feed the same recorder/trace.
//! `#[dial9::main]` builds on this.
//!
//! dial9 builds the runtime because Tokio's hook configuration freezes at
//! `.build()`. Your closure configures the builder and may swap runtime flavor.
//!
//! ```no_run
//! use std::time::Duration;
//! use dial9_core::buffer::DiskBuffer;
//! use dial9_core::recorder::recorder;
//! use dial9_tokio_telemetry::block_on;
//! use dial9_tokio_telemetry::telemetry::{spawn, RecorderTokioExt, TokioAttachOptions};
//!
//! # fn main() -> std::io::Result<()> {
//! let rec = recorder(DiskBuffer::single_file("/tmp/trace.bin")?).build();
//!
//! let (rec, main_rt) = rec.attach_tokio_runtime_with(
//!     TokioAttachOptions::builder().runtime_name("main").build(),
//!     |t| { t.worker_threads(4); },
//! )?;
//!
//! let (rec, io_rt) = rec.attach_tokio_runtime_with(
//!     TokioAttachOptions::builder().runtime_name("io").build(),
//!     |t| { t.worker_threads(2); },
//! )?;
//!
//! block_on(&main_rt, async { spawn(async { /* work */ }).await.unwrap() });
//!
//! // graceful_shutdown is synchronous; drop the runtimes first so their workers
//! // flush, then drain.
//! drop(main_rt);
//! drop(io_rt);
//! rec.graceful_shutdown(Duration::from_secs(5));
//! # Ok(())
//! # }
//! ```
//!
//! Lost shutdown telemetry is acceptable in this model; `#[dial9::main]` handles
//! runtime-drop-before-shutdown ordering for you.

use super::register_runtime_context;
use super::runtime_context::{RuntimeContextRegistry, TokioRuntimesSource};
use crate::primitives::sync::{Arc, Mutex};
use crate::rate_limit::rate_limited;
use crate::telemetry::recorder::runtime_context::register_runtime_metrics;
use crate::telemetry::task_dump_config::TaskDumpConfig;
use dial9_core::buffer::BufferMode;
use dial9_core::handle::set_tl_handle;
use dial9_core::recorder::RecorderBuilder;
use dial9_core::recording::Recorder;
use dial9_core::shared_state::SharedState;
use std::io;
use std::time::Duration;

/// dial9 pipeline presets on the recorder builder.
///
/// The default pipeline is automatic: source-requested stages (for example CPU
/// sample symbolization) run at build-time wiring, then segments are compressed
/// and written back. If there are no stages and no upload target, no worker
/// starts. These methods customize that behavior.
pub trait RecorderPipelineExt<M: BufferMode>: Sized {
    /// Replace the default pipeline with your own processors, run verbatim.
    ///
    /// Nothing is added for you, so chain
    /// [`symbolize`](crate::background_task::PipelineBuilder::symbolize)
    /// yourself if the trace has CPU samples. Disk-only steps like
    /// [`write_back`](crate::background_task::PipelineBuilder::write_back) are
    /// out of scope on a memory writer (compile error).
    fn with_custom_pipeline<F>(self, build: F) -> Self
    where
        F: FnOnce(
            crate::background_task::PipelineBuilder<M>,
        ) -> crate::background_task::PipelineBuilder<M>;

    /// Upload sealed segments to S3 instead of writing them back, using the
    /// default AWS credential chain.
    #[cfg(feature = "worker-s3")]
    fn with_s3_uploader(self, config: crate::background_task::s3::S3Config) -> Self;

    /// Like [`with_s3_uploader`](Self::with_s3_uploader), but with a pre-built S3
    /// client (custom credentials, endpoint, or a test double).
    #[cfg(feature = "worker-s3")]
    fn with_s3_uploader_client(
        self,
        config: crate::background_task::s3::S3Config,
        client: aws_sdk_s3::Client,
    ) -> Self;
}

impl<M: BufferMode> RecorderPipelineExt<M> for RecorderBuilder<M> {
    fn with_custom_pipeline<F>(self, build: F) -> Self
    where
        F: FnOnce(
            crate::background_task::PipelineBuilder<M>,
        ) -> crate::background_task::PipelineBuilder<M>,
    {
        let processors = build(crate::background_task::PipelineBuilder::new()).into_processors();
        self.processors(processors)
    }

    #[cfg(feature = "worker-s3")]
    fn with_s3_uploader(self, config: crate::background_task::s3::S3Config) -> Self {
        apply_s3_uploader(self, config, |uploader| uploader)
    }

    #[cfg(feature = "worker-s3")]
    fn with_s3_uploader_client(
        self,
        config: crate::background_task::s3::S3Config,
        client: aws_sdk_s3::Client,
    ) -> Self {
        apply_s3_uploader(self, config, |mut uploader| {
            uploader.set_client(client);
            uploader
        })
    }
}

/// Asynchronous S3 client construction on the recorder's pipeline worker.
///
/// This is separate from [`RecorderPipelineExt`] to keep that existing public
/// trait unchanged.
#[cfg(feature = "worker-s3")]
pub trait RecorderS3ClientExt<M: BufferMode>:
    recorder_s3_client_ext_sealed::Sealed + Sized
{
    /// Like [`RecorderPipelineExt::with_s3_uploader`], but constructs the S3
    /// client asynchronously on the pipeline worker's Tokio runtime.
    fn with_s3_uploader_client_future<F>(
        self,
        config: crate::background_task::s3::S3Config,
        client_future: F,
    ) -> Self
    where
        F: std::future::Future<Output = aws_sdk_s3::Client> + Send + 'static;
}

#[cfg(feature = "worker-s3")]
mod recorder_s3_client_ext_sealed {
    use dial9_core::buffer::BufferMode;
    use dial9_core::recorder::RecorderBuilder;

    pub trait Sealed {}
    impl<M: BufferMode> Sealed for RecorderBuilder<M> {}
}

#[cfg(feature = "worker-s3")]
impl<M: BufferMode> RecorderS3ClientExt<M> for RecorderBuilder<M> {
    fn with_s3_uploader_client_future<F>(
        self,
        config: crate::background_task::s3::S3Config,
        client_future: F,
    ) -> Self
    where
        F: std::future::Future<Output = aws_sdk_s3::Client> + Send + 'static,
    {
        apply_s3_uploader(self, config, |uploader| {
            uploader.with_client_future(client_future)
        })
    }
}

/// Make S3 the pipeline's terminal stage and fold the S3 config (bucket,
/// service, prefix, region) into the recorder's segment metadata, so consumers
/// can see where a trace was uploaded.
#[cfg(feature = "worker-s3")]
fn apply_s3_uploader<M: BufferMode>(
    builder: RecorderBuilder<M>,
    config: crate::background_task::s3::S3Config,
    configure: impl FnOnce(
        crate::background_task::S3PipelineUploader,
    ) -> crate::background_task::S3PipelineUploader,
) -> RecorderBuilder<M> {
    let metadata: Vec<(String, String)> = config
        .as_metadata()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let boot_id = builder.writer_boot_id().map(str::to_owned);
    let uploader = crate::background_task::S3PipelineUploader::new(config, None);
    let mut uploader = configure(uploader);
    if let Some(boot_id) = boot_id {
        uploader.set_boot_id(boot_id);
    }
    builder
        .segment_metadata(metadata)
        .terminal_processor(uploader)
}

/// Per-runtime attach settings. All optional; runtime-scoped only (session-wide
/// settings like the pipeline, S3, and segment metadata stay on the recorder).
#[derive(bon::Builder)]
pub struct TokioAttachOptions {
    /// Human-readable runtime name, recorded into segment metadata as
    /// `runtime.{name}`.
    #[builder(into)]
    runtime_name: Option<String>,
    /// Install dial9's Tokio runtime hooks. When `false`, attaching is a no-op
    /// and the runtime you build records nothing. Default `true`.
    #[builder(default = true)]
    tokio_instrumentation_enabled: bool,
    /// Record task spawn/terminate events for this runtime. Default `false`.
    #[builder(default)]
    task_tracking_enabled: bool,
    /// Async-backtrace capture config (requires the `taskdump` feature).
    task_dump_config: Option<TaskDumpConfig>,
    /// User-composed Tokio hooks, run after dial9's own.
    #[builder(default)]
    tokio_hooks: super::TokioHooks,
}

impl Default for TokioAttachOptions {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl std::fmt::Debug for TokioAttachOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioAttachOptions")
            .field("runtime_name", &self.runtime_name)
            .field(
                "tokio_instrumentation_enabled",
                &self.tokio_instrumentation_enabled,
            )
            .field("task_tracking_enabled", &self.task_tracking_enabled)
            .finish_non_exhaustive()
    }
}

/// A recorder and a Tokio runtime instrumented against it, as returned by
/// [`RecorderTokioExt::attach_tokio_runtime`].
///
/// Name it in a `#[dial9::main]` config function's signature:
///
/// ```text
/// fn my_config() -> std::io::Result<dial9::AttachedRuntime>
/// ```
pub type AttachedRuntime = (Recorder, tokio::runtime::Runtime);

/// Build Tokio runtimes instrumented against a recorder. Implemented for the
/// core [`Recorder`]; the `dial9` facade re-exports it.
pub trait RecorderTokioExt {
    /// Attach a Tokio runtime to this recorder and return both as an
    /// [`AttachedRuntime`].
    ///
    /// `configure` receives a `tokio::runtime::Builder` pre-seeded with
    /// `enable_all()`. Use it to set `worker_threads`, `thread_name`, etc., or
    /// replace `*t` to switch flavor (for example
    /// `*t = Builder::new_current_thread()`).
    ///
    /// Each call attaches another runtime (it does not replace earlier ones):
    ///
    /// ```ignore
    /// let (rec, api) = rec.attach_tokio_runtime(|t| { t.worker_threads(4); })?;
    /// let (rec, io)  = rec.attach_tokio_runtime(|t| { t.worker_threads(2); })?;
    /// ```
    ///
    /// On a disabled recorder this still returns a working, untraced runtime.
    ///
    /// Do not set builder thread/task callbacks directly (`t.on_thread_start`)
    /// because dial9 must install its own hooks. Use
    /// [`TokioHooks`](super::TokioHooks) via
    /// [`attach_tokio_runtime_with`](Self::attach_tokio_runtime_with) to compose yours.
    ///
    /// Drop the runtime before
    /// [`Recorder::graceful_shutdown`](dial9_core::recording::Recorder::graceful_shutdown)
    /// so its workers flush.
    ///
    /// Drive the root future with [`block_on`](crate::block_on), not
    /// [`Runtime::block_on`](tokio::runtime::Runtime::block_on): to ensure polls and wakes
    /// are captured.
    ///
    /// ```no_run
    /// use dial9_core::buffer::MemoryBuffer;
    /// use dial9_core::recorder::recorder;
    /// use dial9_tokio_telemetry::telemetry::RecorderTokioExt;
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let (recorder, runtime) = recorder(MemoryBuffer::new(1 << 20)?)
    ///     .build()
    ///     .attach_tokio_runtime(|t| { t.worker_threads(4); })?;
    /// # let _ = (recorder, runtime);
    /// # Ok(())
    /// # }
    /// ```
    fn attach_tokio_runtime(
        self,
        configure: impl FnOnce(&mut tokio::runtime::Builder),
    ) -> io::Result<AttachedRuntime>
    where
        Self: Sized,
    {
        self.attach_tokio_runtime_with(TokioAttachOptions::default(), configure)
    }

    /// [`attach_tokio_runtime`](Self::attach_tokio_runtime) with dial9 per-runtime options.
    ///
    /// - `options`: dial9 tracing behavior (runtime name, task tracking,
    ///   task-dump config, composed hooks).
    /// - `configure`: Tokio runtime construction (worker count, thread names,
    ///   flavor).
    ///
    /// `tokio_instrumentation_enabled(false)` skips instrumentation for this
    /// runtime only; other recorder-attached runtimes are unaffected.
    fn attach_tokio_runtime_with(
        self,
        options: TokioAttachOptions,
        configure: impl FnOnce(&mut tokio::runtime::Builder),
    ) -> io::Result<AttachedRuntime>
    where
        Self: Sized;
}

impl RecorderTokioExt for Recorder {
    fn attach_tokio_runtime_with(
        self,
        options: TokioAttachOptions,
        configure: impl FnOnce(&mut tokio::runtime::Builder),
    ) -> io::Result<AttachedRuntime> {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();
        configure(&mut builder);
        let instrumented = options.tokio_instrumentation_enabled;
        attach_tokio_builder(&self, &mut builder, options);
        let runtime = builder.build()?;

        if let Some(shared) = self.shared()
            && instrumented
        {
            register_runtime_metrics(shared, runtime.handle().metrics());
        }
        Ok((self, runtime))
    }
}

/// Register dial9 hooks on a caller-owned Tokio builder and wire it into the
/// recorder's shared runtime-context source. Worker IDs are reserved lazily on
/// first poll.
///
/// [`RecorderTokioExt::attach_tokio_runtime`] is the public API that also builds the
/// runtime.
pub(crate) fn attach_tokio_builder(
    recorder: &Recorder,
    builder: &mut tokio::runtime::Builder,
    options: TokioAttachOptions,
) {
    let Some(shared) = recorder.shared() else {
        return;
    };
    if !options.tokio_instrumentation_enabled {
        return;
    }
    let Some(registry) = runtime_registry(shared) else {
        rate_limited!(Duration::from_secs(60), {
            tracing::error!(
                target: "dial9_telemetry",
                "source registry unavailable; Tokio runtime not attached"
            );
        });
        return;
    };

    let task_dump_config = options.task_dump_config;
    register_runtime_context(
        shared,
        &registry,
        builder,
        options.runtime_name,
        recorder.handle(),
        options.task_tracking_enabled,
        options.tokio_hooks,
        task_dump_config,
    );
    // Install the recorder handle on this thread. For a current_thread runtime
    // the caller builds and drives it on this same thread, which gets no
    // on_thread_start, so the tracing layer and `dial9::spawn` would otherwise
    // find no handle until the first task poll.
    set_tl_handle(recorder.handle().clone());
    // Same for the task-dump config: on_thread_start skips this thread.
    #[cfg(feature = "taskdump")]
    if let Some(config) = task_dump_config {
        crate::task_dumped::set_taskdump_config(config);
    }
}

/// The recorder's runtime registry, installing the [`TokioRuntimesSource`] that
/// owns it on first use. Find-or-insert under one lock, so racing attaches share
/// one source. `None` only if the source lock is poisoned.
pub(crate) fn runtime_registry(shared: &Arc<SharedState>) -> Option<RuntimeContextRegistry> {
    shared.with_sources_vec(|sources| {
        let existing = sources.iter_mut().find_map(|source| {
            let any: &mut dyn std::any::Any = &mut **source;
            any.downcast_mut::<TokioRuntimesSource>()
        });
        if let Some(source) = existing {
            return source.registry().clone();
        }
        let registry: RuntimeContextRegistry = Arc::new(Mutex::new(Vec::new()));
        sources.push(Box::new(TokioRuntimesSource::new(registry.clone())));
        registry
    })
}
