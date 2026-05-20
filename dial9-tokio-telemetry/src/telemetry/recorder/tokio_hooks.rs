use crate::primitives::sync::Arc;

type NoArgHook = Option<Arc<dyn Fn() + Send + Sync>>;
type TaskMetaHook = Option<Arc<dyn Fn(&tokio::runtime::TaskMeta<'_>) + Send + Sync>>;

/// User-provided callbacks to run alongside dial9's internal Tokio runtime hooks.
///
/// All callbacks are composed with dial9's internal hooks: dial9 always runs
/// first, then the user callback fires. This applies to all 8 hooks.
///
/// # Example
///
/// ```rust,no_run
/// use dial9_tokio_telemetry::telemetry::{TokioHooks, TracedRuntime, NullWriter};
///
/// let mut builder = tokio::runtime::Builder::new_multi_thread();
/// builder.worker_threads(4).enable_all();
/// let (runtime, guard) = TracedRuntime::builder()
///     .with_tokio_hooks(|h| {
///         h.on_thread_start(|| println!("started"));
///         h.on_thread_stop(|| println!("stopping"));
///     })
///     .build_and_start_with_writer(builder, NullWriter)
///     .unwrap();
/// ```
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct TokioHooks {
    pub(crate) on_thread_start: NoArgHook,
    pub(crate) on_thread_stop: NoArgHook,
    pub(crate) on_thread_park: NoArgHook,
    pub(crate) on_thread_unpark: NoArgHook,
    pub(crate) on_task_spawn: TaskMetaHook,
    pub(crate) on_task_terminate: TaskMetaHook,
    pub(crate) on_before_task_poll: TaskMetaHook,
    pub(crate) on_after_task_poll: TaskMetaHook,
}

impl TokioHooks {
    /// Set a callback to run when a runtime worker thread starts.
    pub fn on_thread_start(&mut self, f: impl Fn() + Send + Sync + 'static) -> &mut Self {
        self.on_thread_start = Some(Arc::new(f));
        self
    }

    /// Set a callback to run when a runtime worker thread stops.
    pub fn on_thread_stop(&mut self, f: impl Fn() + Send + Sync + 'static) -> &mut Self {
        self.on_thread_stop = Some(Arc::new(f));
        self
    }

    /// Set a callback to run when a runtime worker thread parks.
    pub fn on_thread_park(&mut self, f: impl Fn() + Send + Sync + 'static) -> &mut Self {
        self.on_thread_park = Some(Arc::new(f));
        self
    }

    /// Set a callback to run when a runtime worker thread unparks.
    pub fn on_thread_unpark(&mut self, f: impl Fn() + Send + Sync + 'static) -> &mut Self {
        self.on_thread_unpark = Some(Arc::new(f));
        self
    }

    /// Set a callback to run when a task is spawned.
    pub fn on_task_spawn(
        &mut self,
        f: impl Fn(&tokio::runtime::TaskMeta<'_>) + Send + Sync + 'static,
    ) -> &mut Self {
        self.on_task_spawn = Some(Arc::new(f));
        self
    }

    /// Set a callback to run when a task terminates.
    pub fn on_task_terminate(
        &mut self,
        f: impl Fn(&tokio::runtime::TaskMeta<'_>) + Send + Sync + 'static,
    ) -> &mut Self {
        self.on_task_terminate = Some(Arc::new(f));
        self
    }

    /// Set a callback to run before a task is polled.
    pub fn on_before_task_poll(
        &mut self,
        f: impl Fn(&tokio::runtime::TaskMeta<'_>) + Send + Sync + 'static,
    ) -> &mut Self {
        self.on_before_task_poll = Some(Arc::new(f));
        self
    }

    /// Set a callback to run after a task is polled.
    pub fn on_after_task_poll(
        &mut self,
        f: impl Fn(&tokio::runtime::TaskMeta<'_>) + Send + Sync + 'static,
    ) -> &mut Self {
        self.on_after_task_poll = Some(Arc::new(f));
        self
    }
}

impl std::fmt::Debug for TokioHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioHooks")
            .field("on_thread_start", &self.on_thread_start.is_some())
            .field("on_thread_stop", &self.on_thread_stop.is_some())
            .field("on_thread_park", &self.on_thread_park.is_some())
            .field("on_thread_unpark", &self.on_thread_unpark.is_some())
            .field("on_task_spawn", &self.on_task_spawn.is_some())
            .field("on_task_terminate", &self.on_task_terminate.is_some())
            .field("on_before_task_poll", &self.on_before_task_poll.is_some())
            .field("on_after_task_poll", &self.on_after_task_poll.is_some())
            .finish()
    }
}
