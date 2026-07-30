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
//! Lost shutdown telemetry is acceptable in this model; `#[dial9::main]` handles
//! runtime-drop-before-shutdown ordering for you.

use super::register_runtime_context;
use super::runtime_context::{RuntimeContextRegistry, TokioRuntimesSource};
use crate::primitives::sync::{Arc, Mutex};
use crate::rate_limit::rate_limited;
use crate::telemetry::recorder::runtime_context::register_runtime_metrics;
use crate::telemetry::task_dump_config::TaskDumpConfig;
use dial9_core::buffer::BufferMode;
use dial9_core::handle::{Dial9Handle, set_tl_handle};
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

/// The recorder and runtime pair a `#[dial9::main]` config returns.
///
/// Name it in a `#[dial9::main]` config function's signature:
///
/// ```text
/// fn my_config() -> std::io::Result<dial9::AttachedRuntime>
/// ```
pub type AttachedRuntime = (Recorder, tokio::runtime::Runtime);

/// Tokio instrumentation for [`Dial9Handle`].
pub trait Dial9HandleTokioExt {
    /// Instrument a builder you configured, build it, and return the runtime.
    ///
    /// Get a handle from [`Recorder::handle`](dial9_core::recording::Recorder::handle),
    /// or clone one per thread. Each call attaches another runtime (it does not
    /// replace earlier ones) and every runtime attached to the same recorder
    /// records into the same trace.
    ///
    /// - `builder`: yours to configure. dial9 does not seed it, so call
    ///   `enable_all` (or the drivers you need) yourself, and pick the flavor.
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
    ///             runtime.block_on(async { /* ... */ });
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

impl Dial9HandleTokioExt for Dial9Handle {
    fn attach_tokio_runtime(
        &self,
        mut builder: tokio::runtime::Builder,
        options: TokioAttachOptions,
    ) -> io::Result<tokio::runtime::Runtime> {
        let instrumented = options.tokio_instrumentation_enabled;
        install_tokio_hooks(self, &mut builder, options);
        let runtime = builder.build()?;

        if let Some(shared) = self.shared()
            && instrumented
        {
            register_runtime_metrics(shared, runtime.handle().metrics());
        }
        Ok(runtime)
    }
}

/// Register dial9 hooks on a caller-owned Tokio builder and wire it into the
/// recorder's shared runtime-context source. Worker IDs are reserved lazily on
/// first poll.
///
/// [`Dial9HandleTokioExt::attach_tokio_runtime`] is the public API that also
/// builds the runtime.
pub(crate) fn install_tokio_hooks(
    handle: &Dial9Handle,
    builder: &mut tokio::runtime::Builder,
    options: TokioAttachOptions,
) {
    let Some(shared) = handle.shared() else {
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
        handle,
        options.task_tracking_enabled,
        options.tokio_hooks,
        task_dump_config,
    );
    // Install the handle on this thread. For a current_thread runtime the
    // caller builds and drives it on this same thread, which gets no
    // on_thread_start, so the tracing layer and `dial9::spawn` would otherwise
    // find no handle until the first task poll.
    set_tl_handle(handle.clone());
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
