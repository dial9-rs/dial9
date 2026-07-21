use super::attach_runtime;
use super::builder::TracedRuntime;
use super::handle::Dial9TokioHandle;

/// A pending runtime attachment, returned by [`TracedRuntime::trace_runtime`].
///
/// Chain per-runtime settings on it, then finish with
/// [`build`](Self::build), passing a [`tokio::runtime::Builder`], to install the
/// hooks and create the runtime.
#[must_use]
#[derive(Debug)]
pub struct RuntimeAttach<'a> {
    traced: &'a TracedRuntime,
    name: String,
    task_tracking: bool,
    tokio_instrumentation_enabled: bool,
    tokio_hooks: super::TokioHooks,
}

impl<'a> RuntimeAttach<'a> {
    pub(crate) fn new(traced: &'a TracedRuntime, name: String) -> Self {
        Self {
            traced,
            name,
            task_tracking: false,
            tokio_instrumentation_enabled: true,
            tokio_hooks: super::TokioHooks::default(),
        }
    }

    /// Enable or disable task spawn/terminate tracking for this runtime.
    /// Defaults to `false`.
    pub fn task_tracking(mut self, enabled: bool) -> Self {
        self.task_tracking = enabled;
        self
    }

    /// Enable or disable dial9's Tokio runtime instrumentation for this runtime.
    /// Defaults to `true`.
    pub fn with_tokio_instrumentation(mut self, enabled: bool) -> Self {
        self.tokio_instrumentation_enabled = enabled;
        self
    }

    /// Configure user-provided callbacks to run alongside dial9's internal
    /// Tokio runtime hooks. dial9's logic always runs first, then the user
    /// callbacks fire in registration order.
    pub fn with_tokio_hooks<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut super::TokioHooks),
    {
        f(&mut self.tokio_hooks);
        self
    }

    /// Install telemetry hooks, build the runtime, and reserve worker IDs.
    ///
    /// Returns the runtime and a [`Dial9TokioHandle`] for spawning
    /// instrumented futures via [`Dial9TokioHandle::spawn`]. If Tokio
    /// instrumentation is disabled, builds a plain runtime instead.
    pub fn build(
        self,
        mut builder: tokio::runtime::Builder,
    ) -> std::io::Result<(tokio::runtime::Runtime, Dial9TokioHandle)> {
        let (Some(shared), Some(contexts), Some(session_handle), Some(traced)) = (
            self.traced.shared(),
            self.traced.contexts_registry(),
            self.traced.session_handle(),
            super::traced_handle(&self.traced.record_handle()),
        ) else {
            // Disabled recorder: build a plain tokio runtime and return a
            // Dial9TokioHandle that effectively short-circuits to tokio::spawn.
            let runtime = builder.build()?;
            let handle = Dial9TokioHandle::for_runtime(runtime.handle().clone(), None);
            return Ok((runtime, handle));
        };

        if !self.tokio_instrumentation_enabled {
            let runtime = builder.build()?;
            let handle = Dial9TokioHandle::for_runtime(runtime.handle().clone(), None);
            return Ok((runtime, handle));
        }

        let runtime = attach_runtime(
            shared,
            contexts,
            builder,
            Some(self.name),
            session_handle,
            self.task_tracking,
            self.tokio_hooks,
            self.traced.taskdump_config(),
        )?;
        let handle = Dial9TokioHandle::for_runtime(runtime.handle().clone(), Some(traced));
        Ok((runtime, handle))
    }
}
