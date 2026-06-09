use crate::primitives::sync::Arc;
use crate::telemetry::buffer;
use std::time::Duration;

use super::handle::{RuntimeTelemetryHandle, TelemetryHandle};
use super::shared_state::SharedState;
use super::{ControlCommand, attach_runtime};

/// Holds the background worker thread and its stop signal.
pub(crate) struct WorkerHandle {
    pub(super) shutdown: Option<tokio::sync::oneshot::Sender<Duration>>,
    pub(super) thread: Option<crate::primitives::thread::JoinHandle<()>>,
}

/// RAII guard returned by [`TracedRuntimeBuilder::build`](super::builder::TracedRuntimeBuilder::build).
///
/// A guard is always present on a [`TracedRuntime`](super::builder::TracedRuntime), regardless of
/// whether telemetry is enabled. When telemetry is disabled (because
/// the user opted out via `enabled(false)` or because a lenient config
/// path downgraded after a build failure), the guard is in an inert
/// mode: all methods are no-ops, [`handle`](Self::handle) returns an
/// inert [`TelemetryHandle`], and [`graceful_shutdown`](Self::graceful_shutdown)
/// is a successful no-op.
///
/// Use [`is_enabled`](Self::is_enabled) to distinguish the two modes.
pub struct TelemetryGuard {
    inner: GuardInner,
}

enum GuardInner {
    Enabled(EnabledGuard),
    Disabled,
}

struct EnabledGuard {
    handle: TelemetryHandle,
    flush_thread: Option<crate::primitives::thread::JoinHandle<()>>,
    worker: Option<WorkerHandle>,
}

impl std::fmt::Debug for TelemetryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryGuard")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl TelemetryGuard {
    pub(crate) fn enabled(
        handle: TelemetryHandle,
        flush_thread: Option<crate::primitives::thread::JoinHandle<()>>,
        worker: Option<WorkerHandle>,
    ) -> Self {
        Self {
            inner: GuardInner::Enabled(EnabledGuard {
                handle,
                flush_thread,
                worker,
            }),
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            inner: GuardInner::Disabled,
        }
    }

    /// Whether this guard owns a live telemetry session.
    ///
    /// Returns `false` for guards created by `enabled(false)` configs
    /// or by lenient configs that downgraded after a build failure.
    pub fn is_enabled(&self) -> bool {
        matches!(self.inner, GuardInner::Enabled(_))
    }

    /// Get a cloneable handle for controlling telemetry.
    ///
    /// On a disabled guard this returns an inert handle whose methods
    /// are all no-ops — see [`TelemetryHandle::disabled`].
    pub fn handle(&self) -> TelemetryHandle {
        match &self.inner {
            GuardInner::Enabled(eg) => eg.handle.clone(),
            GuardInner::Disabled => TelemetryHandle::disabled(),
        }
    }

    /// Monotonic start time of the telemetry session in nanoseconds, if
    /// telemetry is enabled.
    pub fn start_time(&self) -> Option<u64> {
        self.shared().map(|s| s.start_time_ns)
    }

    /// Enable telemetry recording. No-op on a disabled guard.
    pub fn enable(&self) {
        if let GuardInner::Enabled(eg) = &self.inner {
            eg.handle.enable();
        }
    }

    /// Disable telemetry recording. No-op on a disabled guard.
    pub fn disable(&self) {
        if let GuardInner::Enabled(eg) = &self.inner {
            eg.handle.disable();
        }
    }

    /// Access the shared state for reuse by additional runtimes.
    pub(crate) fn shared(&self) -> Option<&Arc<SharedState>> {
        match &self.inner {
            GuardInner::Enabled(eg) => eg.handle.shared(),
            GuardInner::Disabled => None,
        }
    }

    pub(crate) fn control_tx(
        &self,
    ) -> Option<&crate::primitives::sync::mpsc::SyncSender<ControlCommand>> {
        match &self.inner {
            GuardInner::Enabled(eg) => eg.handle.control_tx(),
            GuardInner::Disabled => None,
        }
    }

    /// Attach a tokio runtime to this telemetry session.
    ///
    /// Returns a builder that lets you configure per-runtime settings
    /// (e.g. task tracking) before building the runtime.
    ///
    /// On a disabled guard the resulting builder produces a plain tokio
    /// runtime with no telemetry hooks installed.
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::{NullWriter, TelemetryCore};
    /// # fn main() -> std::io::Result<()> {
    /// let guard = TelemetryCore::builder()
    ///     .writer(NullWriter)
    ///     .build()?;
    /// guard.enable();
    ///
    /// let mut builder = tokio::runtime::Builder::new_multi_thread();
    /// builder.worker_threads(4).enable_all();
    /// let (runtime, handle) = guard.trace_runtime("main").build(builder)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn trace_runtime(&self, name: impl Into<String>) -> TraceRuntimeCoreBuilder<'_> {
        TraceRuntimeCoreBuilder {
            guard: self,
            name: name.into(),
            task_tracking: false,
            tokio_instrumentation_enabled: true,
            custom_event_sources: Vec::new(),
            tokio_hooks: super::TokioHooks::default(),
        }
    }

    /// Send FinalizeAndStop to the flush thread, join it, then drain the
    /// caller's thread-local buffer into the collector so the flush thread
    /// picks up any stragglers. No-op when telemetry is disabled.
    fn stop_flush_thread(&mut self) {
        let GuardInner::Enabled(eg) = &mut self.inner else {
            return;
        };
        // Drain the current thread's buffer (e.g. main thread in block_on)
        // which may contain TaskSpawn events that were never flushed.
        if let Some(shared) = eg.handle.shared() {
            buffer::drain_to_collector(&shared.collector);
        }

        // Tell the flush thread to do a final flush + finalize, then exit.
        let (ack_tx, ack_rx) = crate::primitives::sync::mpsc::sync_channel(0);
        if let Some(tx) = eg.handle.control_tx()
            && tx.send(ControlCommand::FinalizeAndStop(ack_tx)).is_ok()
        {
            let _ = ack_rx.recv();
        }
        if let Some(t) = eg.flush_thread.take() {
            let _ = t.join();
        }
    }

    /// Flush remaining events, seal the final segment, and wait for the
    /// background worker to drain (symbolize, compress, upload to S3).
    ///
    /// **Call this after the runtime has been dropped** so that Tokio worker
    /// threads have exited and their thread-local telemetry buffers have been
    /// flushed to the central collector.
    ///
    /// On a disabled guard this is a successful no-op — there is no
    /// flush thread or background worker to drain.
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::{DiskWriter, TracedRuntime};
    /// # use std::time::Duration;
    /// # fn main() -> std::io::Result<()> {
    /// # let writer = DiskWriter::new("/tmp/t.bin", 1024, 4096)?;
    /// # let builder = tokio::runtime::Builder::new_multi_thread();
    /// let (runtime, guard) = TracedRuntime::build_and_start(builder, writer)?;
    /// runtime.block_on(async { /* ... */ });
    /// drop(runtime); // worker threads exit, flushing thread-local buffers
    /// guard.graceful_shutdown(Duration::from_secs(5))?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Consumes the guard so `Drop` becomes a no-op.
    pub fn graceful_shutdown(mut self, timeout: Duration) -> Result<(), std::io::Error> {
        tracing::debug!(target: "dial9_telemetry", "graceful_shutdown starting");

        // 1. Stop flush thread (flushes + finalizes the last segment).
        // No-op when disabled.
        self.stop_flush_thread();
        tracing::debug!(target: "dial9_telemetry", "flush thread joined, segment sealed");

        // 2. Signal worker to drain with the given timeout and wait
        if let GuardInner::Enabled(eg) = &mut self.inner
            && let Some(ref mut w) = eg.worker
        {
            tracing::debug!(target: "dial9_telemetry", timeout_secs = timeout.as_secs(), "waiting for worker drain");
            if let Some(tx) = w.shutdown.take() {
                let _ = tx.send(timeout);
            }
            if let Some(t) = w.thread.take()
                && let Err(e) = t.join()
            {
                tracing::error!(target: "dial9_telemetry", panic = ?e, "worker thread panicked during shutdown");
            }
            tracing::debug!(target: "dial9_telemetry", "worker finished");
        }

        Ok(())
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // 1. Stop the flush thread (flushes + finalizes). No-op when disabled.
        self.stop_flush_thread();

        // 2. Hard shutdown: drop the sender without sending — worker sees
        // RecvError and exits without draining. No need to join the thread.
        // For graceful drain, use graceful_shutdown() instead.
        if let GuardInner::Enabled(eg) = &mut self.inner
            && let Some(ref mut w) = eg.worker
        {
            w.shutdown.take();
        }
    }
}

/// Builder for attaching a runtime to an existing telemetry session.
///
/// Created by [`TelemetryGuard::trace_runtime`]. Call [`.build()`](Self::build)
/// with a [`tokio::runtime::Builder`] to install hooks and build the runtime.
#[must_use]
#[derive(Debug)]
pub struct TraceRuntimeCoreBuilder<'a> {
    guard: &'a TelemetryGuard,
    name: String,
    task_tracking: bool,
    tokio_instrumentation_enabled: bool,
    custom_event_sources: Vec<crate::telemetry::custom_events::CustomEventsSource>,
    tokio_hooks: super::TokioHooks,
}

impl<'a> TraceRuntimeCoreBuilder<'a> {
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

    /// Register a custom event callback.
    ///
    /// The callback runs during flush cycles while telemetry is enabled.
    /// Use [`CustomEventsConfig::minimum_interval`](crate::telemetry::CustomEventsConfig::minimum_interval)
    /// to throttle polling-style callbacks. The default interval is
    /// [`std::time::Duration::ZERO`], which runs the callback on every flush
    /// cycle.
    ///
    /// This method can be called multiple times to configure multiple
    /// callbacks.
    pub fn with_custom_events<F>(
        mut self,
        config: crate::telemetry::CustomEventsConfig,
        callback: F,
    ) -> Self
    where
        F: for<'b> FnMut(&mut crate::telemetry::CustomEventsContext<'b>) + Send + 'static,
    {
        self.custom_event_sources
            .push(crate::telemetry::custom_events::CustomEventsSource::new(
                config, callback,
            ));
        self
    }

    /// Install telemetry hooks, build the runtime, and reserve worker IDs.
    ///
    /// Returns the runtime and a [`RuntimeTelemetryHandle`] for spawning
    /// instrumented futures via [`RuntimeTelemetryHandle::spawn`]. If Tokio
    /// instrumentation is disabled, builds a plain runtime instead.
    pub fn build(
        self,
        mut builder: tokio::runtime::Builder,
    ) -> std::io::Result<(tokio::runtime::Runtime, RuntimeTelemetryHandle)> {
        let (Some(shared), Some(control_tx), Some(traced)) = (
            self.guard.shared(),
            self.guard.control_tx(),
            self.guard.handle().traced_handle(),
        ) else {
            // Disabled guard: build a plain tokio runtime and return a
            // RuntimeTelemetryHandle that effectively short-circuits to
            // tokio::spawn.
            let runtime = builder.build()?;
            let handle = RuntimeTelemetryHandle {
                runtime: runtime.handle().clone(),
                traced: None,
            };
            return Ok((runtime, handle));
        };

        if !self.tokio_instrumentation_enabled {
            let runtime = builder.build()?;
            for source in self.custom_event_sources {
                shared.push_source(Box::new(source));
            }
            let handle = RuntimeTelemetryHandle {
                runtime: runtime.handle().clone(),
                traced: None,
            };
            return Ok((runtime, handle));
        }

        let runtime = attach_runtime(
            shared,
            builder,
            Some(self.name),
            control_tx,
            self.task_tracking,
            self.tokio_hooks,
        )?;
        for source in self.custom_event_sources {
            shared.push_source(Box::new(source));
        }
        let handle = RuntimeTelemetryHandle {
            runtime: runtime.handle().clone(),
            traced: Some(traced),
        };
        Ok((runtime, handle))
    }
}
