use crate::primitives::sync::Arc;
use smallvec::SmallVec;

type NoArgCb = Arc<dyn Fn() + Send + Sync>;
type TaskMetaCb = Arc<dyn Fn(&tokio::runtime::TaskMeta<'_>) + Send + Sync>;

/// A collection of stacked callbacks for a single Tokio runtime hook.
///
/// When executed, callbacks fire in registration order.
#[derive(Clone)]
pub(crate) struct TokioHook<H> {
    callbacks: SmallVec<[H; 1]>,
}

impl<H> TokioHook<H> {
    fn new(cb: H) -> Self {
        Self {
            callbacks: SmallVec::from_buf_and_len([cb], 1),
        }
    }

    fn push(&mut self, cb: H) {
        self.callbacks.push(cb);
    }

    /// Number of registered callbacks.
    pub(crate) fn len(&self) -> usize {
        self.callbacks.len()
    }
}

impl TokioHook<NoArgCb> {
    /// Execute all registered no-arg callbacks in registration order.
    #[inline]
    pub(crate) fn execute(&self) {
        for cb in &self.callbacks {
            cb();
        }
    }
}

impl TokioHook<TaskMetaCb> {
    /// Execute all registered task-meta callbacks in registration order.
    #[inline]
    pub(crate) fn execute(&self, meta: &tokio::runtime::TaskMeta<'_>) {
        for cb in &self.callbacks {
            cb(meta);
        }
    }
}

/// User-provided callbacks to run alongside dial9's internal Tokio runtime hooks.
///
/// All callbacks are composed with dial9's internal hooks: dial9 always runs
/// first, then the user callbacks fire in registration order. This applies to
/// all 8 hooks.
///
/// Registering the same hook multiple times (either within one closure or
/// across multiple `with_tokio_hooks` calls) stacks the callbacks — all
/// registered callbacks will fire.
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
    pub(crate) on_thread_start: Option<TokioHook<NoArgCb>>,
    pub(crate) on_thread_stop: Option<TokioHook<NoArgCb>>,
    pub(crate) on_thread_park: Option<TokioHook<NoArgCb>>,
    pub(crate) on_thread_unpark: Option<TokioHook<NoArgCb>>,
    pub(crate) on_task_spawn: Option<TokioHook<TaskMetaCb>>,
    pub(crate) on_task_terminate: Option<TokioHook<TaskMetaCb>>,
    pub(crate) on_before_task_poll: Option<TokioHook<TaskMetaCb>>,
    pub(crate) on_after_task_poll: Option<TokioHook<TaskMetaCb>>,
}

impl TokioHooks {
    /// Register a callback to run when a runtime worker thread starts.
    ///
    /// Multiple callbacks can be registered; they fire in registration order.
    pub fn on_thread_start(&mut self, f: impl Fn() + Send + Sync + 'static) -> &mut Self {
        match &mut self.on_thread_start {
            Some(hook) => hook.push(Arc::new(f)),
            slot => *slot = Some(TokioHook::new(Arc::new(f))),
        }
        self
    }

    /// Register a callback to run when a runtime worker thread stops.
    ///
    /// Multiple callbacks can be registered; they fire in registration order.
    pub fn on_thread_stop(&mut self, f: impl Fn() + Send + Sync + 'static) -> &mut Self {
        match &mut self.on_thread_stop {
            Some(hook) => hook.push(Arc::new(f)),
            slot => *slot = Some(TokioHook::new(Arc::new(f))),
        }
        self
    }

    /// Register a callback to run when a runtime worker thread parks.
    ///
    /// Multiple callbacks can be registered; they fire in registration order.
    pub fn on_thread_park(&mut self, f: impl Fn() + Send + Sync + 'static) -> &mut Self {
        match &mut self.on_thread_park {
            Some(hook) => hook.push(Arc::new(f)),
            slot => *slot = Some(TokioHook::new(Arc::new(f))),
        }
        self
    }

    /// Register a callback to run when a runtime worker thread unparks.
    ///
    /// Multiple callbacks can be registered; they fire in registration order.
    pub fn on_thread_unpark(&mut self, f: impl Fn() + Send + Sync + 'static) -> &mut Self {
        match &mut self.on_thread_unpark {
            Some(hook) => hook.push(Arc::new(f)),
            slot => *slot = Some(TokioHook::new(Arc::new(f))),
        }
        self
    }

    /// Register a callback to run when a task is spawned.
    ///
    /// Multiple callbacks can be registered; they fire in registration order.
    pub fn on_task_spawn(
        &mut self,
        f: impl Fn(&tokio::runtime::TaskMeta<'_>) + Send + Sync + 'static,
    ) -> &mut Self {
        match &mut self.on_task_spawn {
            Some(hook) => hook.push(Arc::new(f)),
            slot => *slot = Some(TokioHook::new(Arc::new(f))),
        }
        self
    }

    /// Register a callback to run when a task terminates.
    ///
    /// Multiple callbacks can be registered; they fire in registration order.
    pub fn on_task_terminate(
        &mut self,
        f: impl Fn(&tokio::runtime::TaskMeta<'_>) + Send + Sync + 'static,
    ) -> &mut Self {
        match &mut self.on_task_terminate {
            Some(hook) => hook.push(Arc::new(f)),
            slot => *slot = Some(TokioHook::new(Arc::new(f))),
        }
        self
    }

    /// Register a callback to run before a task is polled.
    ///
    /// Multiple callbacks can be registered; they fire in registration order.
    pub fn on_before_task_poll(
        &mut self,
        f: impl Fn(&tokio::runtime::TaskMeta<'_>) + Send + Sync + 'static,
    ) -> &mut Self {
        match &mut self.on_before_task_poll {
            Some(hook) => hook.push(Arc::new(f)),
            slot => *slot = Some(TokioHook::new(Arc::new(f))),
        }
        self
    }

    /// Register a callback to run after a task is polled.
    ///
    /// Multiple callbacks can be registered; they fire in registration order.
    pub fn on_after_task_poll(
        &mut self,
        f: impl Fn(&tokio::runtime::TaskMeta<'_>) + Send + Sync + 'static,
    ) -> &mut Self {
        match &mut self.on_after_task_poll {
            Some(hook) => hook.push(Arc::new(f)),
            slot => *slot = Some(TokioHook::new(Arc::new(f))),
        }
        self
    }
}

impl std::fmt::Debug for TokioHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioHooks")
            .field(
                "on_thread_start",
                &self.on_thread_start.as_ref().map(|h| h.len()),
            )
            .field(
                "on_thread_stop",
                &self.on_thread_stop.as_ref().map(|h| h.len()),
            )
            .field(
                "on_thread_park",
                &self.on_thread_park.as_ref().map(|h| h.len()),
            )
            .field(
                "on_thread_unpark",
                &self.on_thread_unpark.as_ref().map(|h| h.len()),
            )
            .field(
                "on_task_spawn",
                &self.on_task_spawn.as_ref().map(|h| h.len()),
            )
            .field(
                "on_task_terminate",
                &self.on_task_terminate.as_ref().map(|h| h.len()),
            )
            .field(
                "on_before_task_poll",
                &self.on_before_task_poll.as_ref().map(|h| h.len()),
            )
            .field(
                "on_after_task_poll",
                &self.on_after_task_poll.as_ref().map(|h| h.len()),
            )
            .finish()
    }
}
