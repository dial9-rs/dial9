//! Build Tokio runtimes instrumented against a [`Recorder`].
//!
//! [`Dial9HandleTokioExt::attach_tokio_runtime`] registers dial9 hooks on a
//! `tokio::runtime::Builder`, builds the runtime, and returns it. Call it once
//! per runtime, all runtimes attached to the same recorder feed one trace.
//! `#[dial9::main]` builds on this.
//!
//! Attach goes through [`Dial9Handle`], so configure the builder, then hand it
//! over. dial9 builds it for you because Tokio's hook configuration freezes at
//! `.build()`. A handle is cheap to clone and `Send`, so it can be used in services
//! where multiple threads build their own runtimes.
//!
//! ```no_run
//! use std::time::Duration;
//! use dial9_core::buffer::DiskBuffer;
//! use dial9_core::recorder::recorder;
//! use dial9_tokio_telemetry::block_on;
//! use dial9_tokio_telemetry::telemetry::{spawn, Dial9HandleTokioExt, TokioAttachOptions};
//!
//! # fn main() -> std::io::Result<()> {
//! let rec = recorder(DiskBuffer::single_file("/tmp/trace.bin")?).build();
//!
//! let mut main_builder = tokio::runtime::Builder::new_multi_thread();
//! main_builder.enable_all().worker_threads(4);
//! let main_rt = rec.handle().attach_tokio_runtime(
//!     main_builder,
//!     TokioAttachOptions::builder().runtime_name("main").build(),
//! )?;
//!
//! let mut io_builder = tokio::runtime::Builder::new_multi_thread();
//! io_builder.enable_all().worker_threads(2);
//! let io_rt = rec.handle().attach_tokio_runtime(
//!     io_builder,
//!     TokioAttachOptions::builder().runtime_name("io").build(),
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
//! Shut down in that order or the workers' last events never reach the trace.
//! `#[dial9::main]` does it for you.

use super::register_runtime_hooks;
#[cfg(not(tokio_unstable))]
use super::runtime_context::RuntimeContext;
use super::runtime_context::{RuntimeContextRegistry, TokioRuntimesSource, WorkerIdCounter};
use crate::primitives::sync::{Arc, Mutex};
use crate::telemetry::recorder::runtime_context::register_runtime_metrics;
use crate::telemetry::task_dump_config::TaskDumpConfig;
use dial9_core::buffer::BufferMode;
use dial9_core::handle::{Dial9Handle, set_tl_handle};
use dial9_core::recorder::RecorderBuilder;
use dial9_core::recording::Recorder;
#[cfg(feature = "worker-s3")]
use dial9_destinations_s3::S3Config;
use std::io;

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
    fn with_s3_uploader(self, config: S3Config) -> Self;

    /// Like [`with_s3_uploader`](Self::with_s3_uploader), but with a pre-built S3
    /// client (custom credentials, endpoint, or a test double).
    #[cfg(feature = "worker-s3")]
    fn with_s3_uploader_client(self, config: S3Config, client: aws_sdk_s3::Client) -> Self;
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
    fn with_s3_uploader(self, config: S3Config) -> Self {
        apply_s3_uploader(self, config, |uploader| uploader)
    }

    #[cfg(feature = "worker-s3")]
    fn with_s3_uploader_client(self, config: S3Config, client: aws_sdk_s3::Client) -> Self {
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
    fn with_s3_uploader_client_future<F>(self, config: S3Config, client_future: F) -> Self
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
    fn with_s3_uploader_client_future<F>(self, config: S3Config, client_future: F) -> Self
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
    config: S3Config,
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
#[derive(Clone, bon::Builder)]
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
    ///
    /// These come from Tokio's task hooks, which need `--cfg tokio_unstable`.
    /// Without it no task spawn/terminate events are recorded regardless of what this is
    /// set to.
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

/// The recorder and runtime pair a `#[dial9::main]` config returns.
///
/// Name it in a `#[dial9::main]` config function's signature:
///
/// ```text
/// fn my_config() -> std::io::Result<dial9::AttachedRuntime>
/// ```
pub type AttachedRuntime = (Recorder, tokio::runtime::Runtime);

/// Tokio instrumentation for [`Dial9Handle`].
pub trait Dial9HandleTokioExt: dial9_handle_tokio_ext_sealed::Sealed {
    /// Instrument a builder you configured, build it, and return the runtime.
    ///
    /// Get a handle from [`Recorder::handle`](dial9_core::recording::Recorder::handle),
    /// or clone one per thread. Each call attaches another runtime (it does not
    /// replace earlier ones) and every runtime attached to the same recorder
    /// records into the same trace.
    ///
    /// - `builder`: yours to configure. dial9 does not seed it, so call
    ///   `enable_all` (or the drivers you need) yourself, and pick the flavor.
    ///   Taken by value: the hooks installed on it belong to this one runtime.
    /// - `options`: dial9 tracing behavior for this runtime (runtime name, task
    ///   tracking, task-dump config, composed hooks).
    ///
    /// On a disabled recorder this still returns a working, untraced runtime, as
    /// does `tokio_instrumentation_enabled(false)`, which skips instrumentation
    /// for this runtime only.
    ///
    /// Do not set builder thread/task callbacks directly (`on_thread_start` and
    /// friends) because dial9 installs its own and would overwrite yours. Pass
    /// [`TokioHooks`](super::TokioHooks) in the options to compose them instead.
    ///
    /// Drop the runtime before calling
    /// [`Recorder::graceful_shutdown`](dial9_core::recording::Recorder::graceful_shutdown)
    /// on the recorder this handle came from, so the runtime's workers flush.
    ///
    /// Attach claims the calling thread: the handle is installed
    /// thread-locally, so [`Dial9Handle::current`] and `dial9::spawn` work
    /// there before the first poll, replacing any handle a previous attach
    /// installed. Attach a `current_thread` runtime on the thread that will
    /// drive it.
    ///
    /// Attaching is permanent: contexts and metrics of attached runtimes stay
    /// registered for the recorder's life, even after the runtime drops.
    ///
    /// # Errors
    ///
    /// Fails if Tokio cannot build the runtime, or if the recorder has
    /// already shut down.
    ///
    /// Drive the root future with [`block_on`](crate::block_on), not
    /// [`Runtime::block_on`](tokio::runtime::Runtime::block_on): to ensure polls and wakes
    /// are captured.
    ///
    /// ```no_run
    /// use dial9_core::buffer::MemoryBuffer;
    /// use dial9_core::recorder::recorder;
    /// use dial9_tokio_telemetry::telemetry::{Dial9HandleTokioExt, TokioAttachOptions};
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let recorder = recorder(MemoryBuffer::new(1 << 20)?).build();
    ///
    /// let mut builder = tokio::runtime::Builder::new_multi_thread();
    /// builder.enable_all().worker_threads(4);
    /// let runtime = recorder
    ///     .handle()
    ///     .attach_tokio_runtime(builder, TokioAttachOptions::default())?;
    /// # let _ = (recorder, runtime);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The handle is cheap to clone, so several threads can each attach their
    /// own runtime off the same recorder:
    ///
    /// ```no_run
    /// use dial9_core::buffer::MemoryBuffer;
    /// use dial9_core::recorder::recorder;
    /// use dial9_tokio_telemetry::telemetry::{Dial9HandleTokioExt, TokioAttachOptions};
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let recorder = recorder(MemoryBuffer::new(1 << 20)?).build();
    /// let handle = recorder.handle().clone();
    ///
    /// let threads: Vec<_> = (0..2)
    ///     .map(|_| {
    ///         let handle = handle.clone();
    ///         std::thread::spawn(move || {
    ///             let mut builder = tokio::runtime::Builder::new_current_thread();
    ///             builder.enable_all();
    ///             let runtime = handle
    ///                 .attach_tokio_runtime(builder, TokioAttachOptions::default())
    ///                 .expect("build runtime");
    ///             dial9_tokio_telemetry::block_on(&runtime, async { /* ... */ });
    ///         })
    ///     })
    ///     .collect();
    /// # for t in threads { t.join().unwrap(); }
    /// # Ok(())
    /// # }
    /// ```
    fn attach_tokio_runtime(
        &self,
        builder: tokio::runtime::Builder,
        options: TokioAttachOptions,
    ) -> io::Result<tokio::runtime::Runtime>;
}

mod dial9_handle_tokio_ext_sealed {
    use dial9_core::handle::Dial9Handle;

    pub trait Sealed {}
    impl Sealed for Dial9Handle {}
}

impl Dial9HandleTokioExt for Dial9Handle {
    fn attach_tokio_runtime(
        &self,
        mut builder: tokio::runtime::Builder,
        options: TokioAttachOptions,
    ) -> io::Result<tokio::runtime::Runtime> {
        if !self.is_connected() {
            return builder.build();
        }
        if self.is_stopped() {
            return Err(io::Error::other(
                "recorder has shut down; attach runtimes before graceful_shutdown",
            ));
        }
        if !options.tokio_instrumentation_enabled {
            return builder.build();
        }
        let Some(state) = tokio_attach_state(self) else {
            return Err(io::Error::other(
                "dial9 source registry unavailable; Tokio runtime not attached",
            ));
        };

        let task_dump_config = options.task_dump_config;
        // Capture the runtime name before `options.runtime_name` is moved into
        // `register_runtime_hooks`, so the metrics registration can tag this
        // runtime's samples with its identity.
        let runtime_name = options.runtime_name.clone();
        let ctx = register_runtime_hooks(
            &mut builder,
            options.runtime_name,
            self,
            state.worker_ids,
            options.task_tracking_enabled,
            options.tokio_hooks,
            task_dump_config,
        );

        let runtime = builder.build()?;

        // Publish attach state only after a successful build.
        // `TokioRuntimesSource` picks the runtime up from the registry on its
        // next flush.
        //
        // Bind first: without tokio's task hooks the polls come from the
        // `WakeTraced` wrapper, which finds this context by the runtime id it
        // is running on.
        ctx.bind_runtime(runtime.handle().id());
        state.registry.lock().unwrap().push(ctx);
        // The current-thread driver does not fire `on_thread_start`, so without
        // this the tracing layer and `dial9::spawn` find no handle until the
        // first poll.
        set_tl_handle(self.clone());
        // Same for the task-dump config.
        #[cfg(feature = "taskdump")]
        if let Some(config) = task_dump_config {
            crate::task_dumped::set_taskdump_config(config);
        }
        register_runtime_metrics(self, runtime_name, runtime.handle().metrics());
        Ok(runtime)
    }
}

/// The context instrumenting the tokio runtime this thread is currently
/// running on, if dial9 is attached to it. Resolved by runtime id, so a thread
/// that drives several runtimes gets the right one every time.
#[cfg(not(tokio_unstable))]
pub(crate) fn current_runtime_ctx(handle: &Dial9Handle) -> Option<Arc<RuntimeContext>> {
    let id = tokio::runtime::Handle::try_current().ok()?.id();
    if let Some(ctx) = super::runtime_context::cached_runtime_ctx(id) {
        return Some(ctx);
    }
    let registry = tokio_attach_state(handle)?.registry;
    let ctx = {
        let registry = registry.lock().ok()?;
        registry.iter().find(|c| c.is_runtime(id)).cloned()?
    };
    super::runtime_context::cache_runtime_ctx(id, &ctx);
    Some(ctx)
}

/// What every runtime attached to a recorder shares, owned by its
/// [`TokioRuntimesSource`].
pub(crate) struct TokioAttachState {
    /// The runtimes attached to this recorder.
    pub registry: RuntimeContextRegistry,
    /// Hands out this recorder's global worker-ID blocks.
    pub worker_ids: WorkerIdCounter,
}

/// This recorder's [`TokioAttachState`], installing the source that owns it on
/// first use.`None` once the recorder has shut down, or if its lock is poisoned.
pub(crate) fn tokio_attach_state(handle: &Dial9Handle) -> Option<TokioAttachState> {
    handle.with_source_or_insert(
        || TokioRuntimesSource::new(Arc::new(Mutex::new(Vec::new()))),
        |source| TokioAttachState {
            registry: source.registry().clone(),
            worker_ids: source.worker_id_counter(),
        },
    )
}
