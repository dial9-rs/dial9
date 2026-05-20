use crate::primitives::sync::Arc;
use std::cell::Cell;
use std::cell::RefCell;

use super::ControlCommand;
use super::shared_state::SharedState;

crate::primitives::thread_local! {
    /// Per-thread [`TelemetryHandle`], populated in `on_thread_start` and
    /// cleared in `on_thread_stop`. Enables [`TelemetryHandle::current`].
    pub(super) static CURRENT_HANDLE: RefCell<Option<TelemetryHandle>> = const { RefCell::new(None) };

    /// Nest count for [`InstrumentedSpawnGuard`]. `on_task_spawn` treats
    /// any value `> 0` as an instrumented spawn.
    pub(super) static INSTRUMENTED_SPAWN: Cell<u32> = const { Cell::new(0) };
}

/// Cheap, cloneable handle for controlling telemetry from anywhere.
///
/// A handle may be in one of two modes:
///
/// - **Enabled** — backed by a real telemetry session; methods record
///   events, control recording, and wrap spawned futures with wake
///   tracking.
/// - **Disabled** — an inert sentinel returned by
///   [`TelemetryHandle::disabled`] and by [`TelemetryHandle::current`]
///   when called from a thread that is not owned by a dial9 runtime.
///   All methods are no-ops; [`spawn`](Self::spawn) falls back to
///   [`tokio::spawn`] without wake tracking.
///
/// Use [`is_enabled`](Self::is_enabled) to distinguish the two modes.
#[derive(Clone)]
pub struct TelemetryHandle {
    inner: Option<HandleInner>,
}

#[derive(Clone)]
struct HandleInner {
    shared: Arc<SharedState>,
    control_tx: crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
}

impl std::fmt::Debug for TelemetryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryHandle")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl TelemetryHandle {
    pub(crate) fn enabled(
        shared: Arc<SharedState>,
        control_tx: crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
    ) -> Self {
        Self {
            inner: Some(HandleInner { shared, control_tx }),
        }
    }

    /// Return an inert handle that is not connected to any telemetry
    /// session. All methods are no-ops; [`spawn`](Self::spawn) falls
    /// back to [`tokio::spawn`] without wake tracking.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Whether this handle is connected to a live telemetry session.
    ///
    /// Returns `false` for handles obtained via
    /// [`TelemetryHandle::disabled`], and for handles returned by
    /// [`TelemetryHandle::current`] when called from a thread that is
    /// not owned by a dial9 runtime.
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub(crate) fn shared(&self) -> Option<&Arc<SharedState>> {
        self.inner.as_ref().map(|i| &i.shared)
    }

    pub(crate) fn control_tx(
        &self,
    ) -> Option<&crate::primitives::sync::mpsc::SyncSender<ControlCommand>> {
        self.inner.as_ref().map(|i| &i.control_tx)
    }

    /// Return the [`TelemetryHandle`] for the current thread.
    ///
    /// On threads owned by a dial9 runtime (workers and blocking
    /// threads — installed via the runtime's `on_thread_start` hook,
    /// cleared on `on_thread_stop`) this returns the live handle for
    /// that runtime.
    ///
    /// On any other thread (including the caller of
    /// `runtime.block_on(...)` on a `current_thread` runtime, threads
    /// outside any tokio context, and threads owned by a runtime built
    /// with telemetry disabled) this returns an inert handle whose
    /// methods are all no-ops — see [`TelemetryHandle::disabled`].
    ///
    /// Use [`is_enabled`](Self::is_enabled) when you need to branch on
    /// whether telemetry is actually live on the current thread.
    pub fn current() -> Self {
        CURRENT_HANDLE
            .with(|cell| cell.borrow().clone())
            .unwrap_or_else(Self::disabled)
    }

    /// Return the [`TelemetryHandle`] installed for the current thread,
    /// or `None` if no dial9 runtime has claimed this thread.
    ///
    /// Prefer [`current`](Self::current) instead.
    pub fn try_current() -> Option<Self> {
        CURRENT_HANDLE.with(|cell| cell.borrow().clone())
    }

    /// Enable telemetry recording. No-op on a disabled handle.
    pub fn enable(&self) {
        if let Some(inner) = &self.inner {
            inner
                .shared
                .enabled
                .store(true, crate::primitives::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Disable telemetry recording. No-op on a disabled handle.
    pub fn disable(&self) {
        if let Some(inner) = &self.inner {
            inner
                .shared
                .enabled
                .store(false, crate::primitives::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Get a [`TracedHandle`](crate::traced::TracedHandle) for wrapping
    /// futures with wake tracking, or `None` on a disabled handle.
    pub(crate) fn traced_handle(&self) -> Option<crate::traced::TracedHandle> {
        self.inner.as_ref().map(|i| crate::traced::TracedHandle {
            shared: i.shared.clone(),
        })
    }

    /// Record a user-defined [`Encodable`](crate::telemetry::buffer::Encodable) event.
    ///
    /// No-op on a disabled handle or when recording is paused.
    pub(crate) fn record_encodable_event(&self, event: &dyn crate::telemetry::buffer::Encodable) {
        if let Some(inner) = &self.inner {
            inner
                .shared
                .if_enabled(|buf| buf.record_encodable_event(event));
        }
    }

    /// Run a closure with direct access to the thread-local encoder.
    ///
    /// The closure is only invoked if telemetry is enabled.
    /// No-op on a disabled handle or when recording is paused.
    // TODO(GH-XXX): consider making this public as an alternative to record_event
    // for zero-copy dynamic schema encoding
    pub(crate) fn with_encoder(
        &self,
        f: impl FnOnce(&mut crate::telemetry::buffer::ThreadLocalEncoder<'_>),
    ) {
        if let Some(inner) = &self.inner {
            inner.shared.if_enabled(|buf| buf.with_encoder(f));
        }
    }

    /// Spawn a future on the ambient tokio runtime.
    ///
    /// On an enabled handle, the future is wrapped with wake-event
    /// tracking. On a disabled handle, this is a passthrough to
    /// [`tokio::spawn`].
    ///
    /// # Panics
    ///
    /// Panics if called from outside a tokio runtime context (same
    /// as [`tokio::spawn`]).
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self.traced_handle() {
            Some(traced_handle) => {
                let _guard = InstrumentedSpawnGuard::enter();
                tokio::spawn(crate::telemetry::TracedFuture::new(
                    future,
                    Some(traced_handle),
                ))
            }
            None => tokio::spawn(future),
        }
    }

    /// Spawn an instrumented future through a user-supplied spawn function.
    ///
    /// `spawn_fn` must synchronously perform a real Tokio spawn (or an
    /// equivalent operation) before returning; do not defer the future or run
    /// it with `block_on`. To record the resulting task as instrumented, spawn
    /// on a dial9-traced runtime with task tracking enabled. The closure's
    /// return value is forwarded back to the caller, so you can keep the
    /// [`tokio::task::JoinHandle`], [`tokio::task::AbortHandle`], or whatever
    /// the spawn function returns.
    ///
    /// # Examples
    ///
    /// Spawn into a [`tokio::task::JoinSet`]:
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::TelemetryHandle;
    /// # use tokio::task::JoinSet;
    /// # async fn work() {}
    /// # async fn demo() {
    /// let handle = TelemetryHandle::current();
    /// let mut set: JoinSet<()> = JoinSet::new();
    /// handle.spawn_with(work(), |f| set.spawn(f));
    /// # }
    /// ```
    ///
    /// Spawn into a [`tokio::task::JoinSet`] on a specific runtime:
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::TelemetryHandle;
    /// # use tokio::runtime::Runtime;
    /// # use tokio::task::JoinSet;
    /// # async fn work() {}
    /// # fn demo(runtime: &Runtime) {
    /// let handle = TelemetryHandle::current();
    /// let mut set: JoinSet<()> = JoinSet::new();
    /// handle.spawn_with(work(), |f| set.spawn_on(f, runtime.handle()));
    /// # }
    /// ```
    ///
    /// [`TracedFuture<F>`]: crate::telemetry::TracedFuture
    pub fn spawn_with<F, S>(
        &self,
        future: F,
        spawn_fn: impl FnOnce(crate::telemetry::TracedFuture<F>) -> S,
    ) -> S
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let traced_handle = self.traced_handle();
        let future = crate::telemetry::TracedFuture::new(future, traced_handle.clone());
        match traced_handle {
            Some(_) => {
                let _guard = InstrumentedSpawnGuard::enter();
                spawn_fn(future)
            }
            None => spawn_fn(future),
        }
    }
}

/// Spawn a traced task on the current tokio runtime.
///
/// Like [`tokio::spawn`], but wraps the future with wake-event tracking
/// when called from a thread owned by a dial9 runtime. On other threads,
/// falls back to plain [`tokio::spawn`].
///
/// Equivalent to [`TelemetryHandle::current().spawn(future)`](TelemetryHandle::spawn).
///
/// # Panics
///
/// Panics if called from outside a tokio runtime context (same
/// as [`tokio::spawn`]).
#[track_caller]
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    TelemetryHandle::current().spawn(future)
}

/// RAII guard that increments `INSTRUMENTED_SPAWN` on creation and
/// decrements it on drop, even if the protected closure panics.
pub(super) struct InstrumentedSpawnGuard;

impl InstrumentedSpawnGuard {
    pub(super) fn enter() -> Self {
        INSTRUMENTED_SPAWN.with(|c| c.set(c.get().saturating_add(1)));
        Self
    }
}

impl Drop for InstrumentedSpawnGuard {
    fn drop(&mut self) {
        INSTRUMENTED_SPAWN.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Handle for spawning instrumented futures on a specific runtime.
///
/// Returned by [`TraceRuntimeCoreBuilder::build`](super::guard::TraceRuntimeCoreBuilder::build).
/// Unlike [`TelemetryHandle::spawn`] which uses `tokio::spawn()` (requiring an
/// ambient runtime context), this type targets a specific runtime and works from
/// any thread.
///
/// `Clone` is cheap — both inner handles are `Arc`-based.
#[derive(Clone, Debug)]
pub struct RuntimeTelemetryHandle {
    pub(super) runtime: tokio::runtime::Handle,
    pub(super) traced: Option<crate::traced::TracedHandle>,
}

impl RuntimeTelemetryHandle {
    /// Spawn a future with wake-event tracking on this handle's runtime.
    ///
    /// On a handle obtained from a disabled [`TelemetryGuard`](super::guard::TelemetryGuard),
    /// wake tracking is skipped and the future is spawned plainly.
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self.traced.clone() {
            Some(traced_handle) => {
                let _guard = InstrumentedSpawnGuard::enter();
                self.runtime.spawn(crate::telemetry::TracedFuture::new(
                    future,
                    Some(traced_handle),
                ))
            }
            None => self.runtime.spawn(future),
        }
    }

    /// Spawn an instrumented future through a user-supplied spawn function.
    ///
    /// Mirrors [`TelemetryHandle::spawn_with`] for callers that already hold a
    /// [`RuntimeTelemetryHandle`]. `spawn_fn` must synchronously perform a real
    /// Tokio spawn (or an equivalent operation) before returning; do not defer
    /// the future or run it with `block_on`. To record the resulting task as
    /// instrumented, target a dial9-traced runtime with task tracking enabled,
    /// typically via [`tokio::task::JoinSet::spawn_on`] with the appropriate
    /// [`tokio::runtime::Handle`].
    ///
    /// # Examples
    ///
    /// Spawn into a [`tokio::task::JoinSet`] on a specific runtime:
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::RuntimeTelemetryHandle;
    /// # use tokio::runtime::Runtime;
    /// # use tokio::task::JoinSet;
    /// # async fn work() {}
    /// # fn demo(runtime: &Runtime, handle: RuntimeTelemetryHandle, set: &mut JoinSet<()>) {
    /// handle.spawn_with(work(), |f| set.spawn_on(f, runtime.handle()));
    /// # }
    /// ```
    ///
    /// [`TracedFuture<F>`]: crate::telemetry::TracedFuture
    pub fn spawn_with<F, S>(
        &self,
        future: F,
        spawn_fn: impl FnOnce(crate::telemetry::TracedFuture<F>) -> S,
    ) -> S
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let future = crate::telemetry::TracedFuture::new(future, self.traced.clone());
        match self.traced {
            Some(_) => {
                let _guard = InstrumentedSpawnGuard::enter();
                spawn_fn(future)
            }
            None => spawn_fn(future),
        }
    }
}
