use crate::TracedFuture;
use crate::primitives::sync::Arc;
use crate::traced::TracedHandle;
use std::cell::Cell;
use std::cell::RefCell;

use super::ControlCommand;
use super::shared_state::SharedState;

crate::primitives::thread_local! {
    /// Per-thread [`Dial9Handle`], populated in `on_thread_start` and
    /// cleared in `on_thread_stop`. Backs [`Dial9Handle::current`] and
    /// [`Dial9TokioHandle::current`].
    pub(super) static CURRENT_HANDLE: RefCell<Option<Dial9Handle>> = const { RefCell::new(None) };

    /// Nest count for [`InstrumentedSpawnGuard`]. `on_task_spawn` treats
    /// any value `> 0` as an instrumented spawn.
    pub(super) static INSTRUMENTED_SPAWN: Cell<u32> = const { Cell::new(0) };
}

/// Cheap, cloneable handle for recording events and controlling telemetry.
///
/// For the Tokio handle that spawns instrumented tasks, see [`Dial9TokioHandle`].
///
/// A handle may be in one of two modes:
///
/// - **Enabled** — backed by a real telemetry session; methods record
///   events and control recording.
/// - **Disabled** — an inert sentinel returned by
///   [`Dial9Handle::disabled`] and by [`Dial9Handle::current`]
///   when called from a thread that is not owned by a dial9 runtime.
///   All methods are no-ops.
///
/// Use [`is_enabled`](Self::is_enabled) to distinguish the two modes.
#[derive(Clone)]
pub struct Dial9Handle {
    inner: Option<HandleInner>,
}

#[derive(Clone)]
struct HandleInner {
    shared: Arc<SharedState>,
    control_tx: crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
}

impl std::fmt::Debug for Dial9Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dial9Handle")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl Dial9Handle {
    pub(crate) fn enabled(
        shared: Arc<SharedState>,
        control_tx: crate::primitives::sync::mpsc::SyncSender<ControlCommand>,
    ) -> Self {
        Self {
            inner: Some(HandleInner { shared, control_tx }),
        }
    }

    /// Return an inert handle that is not connected to any telemetry
    /// session. All methods are no-ops.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Whether this handle is connected to a live telemetry session.
    ///
    /// Returns `false` for handles obtained via
    /// [`Dial9Handle::disabled`], and for handles returned by
    /// [`Dial9Handle::current`] when called from a thread that is
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

    /// Return the [`Dial9Handle`] for the current thread.
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
    /// methods are all no-ops — see [`Dial9Handle::disabled`].
    ///
    /// Use [`is_enabled`](Self::is_enabled) when you need to branch on
    /// whether telemetry is actually live on the current thread.
    pub fn current() -> Self {
        CURRENT_HANDLE
            .with(|cell| cell.borrow().clone())
            .unwrap_or_else(Self::disabled)
    }

    /// Return the [`Dial9Handle`] installed for the current thread,
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

    /// Record a custom event into the trace.
    ///
    /// Any type implementing [`dial9_trace_format::TraceEvent`] (typically via
    /// `#[derive(TraceEvent)]`) works directly. No-op on a disabled handle or
    /// when recording is paused.
    pub fn record_event(&self, event: impl crate::telemetry::buffer::Encodable) {
        self.record_encodable_event(&event);
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
}

/// Tokio handle for spawning instrumented tasks.
///
/// Spawned futures are wrapped with wake-event tracking when telemetry is live
/// on this handle. Otherwise they spawn plainly. Obtain one for the current
/// runtime with [`current`](Self::current), or bound to a specific runtime
/// from [`TelemetryGuard::tokio_handle`](super::guard::TelemetryGuard::tokio_handle)
/// or [`trace_runtime`](super::guard::TelemetryGuard::trace_runtime)'s builder.
///
/// This handle only spawns. For recording and control, use [`Dial9Handle`].
#[derive(Clone)]
pub struct Dial9TokioHandle {
    /// `None` spawns on the current runtime (`tokio::spawn`), `Some` targets a
    /// specific runtime and works from any thread.
    runtime: Option<tokio::runtime::Handle>,
    traced: Option<TracedHandle>,
}

impl std::fmt::Debug for Dial9TokioHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dial9TokioHandle")
            .field("enabled", &self.traced.is_some())
            .finish_non_exhaustive()
    }
}

impl Dial9TokioHandle {
    /// Handle that spawns on the **current** tokio runtime (like `tokio::spawn`).
    ///
    /// Wraps spawned futures with wake tracking when the current thread is owned
    /// by a live dial9 runtime, otherwise spawns plainly.
    pub fn current() -> Self {
        Self {
            runtime: None,
            traced: Dial9Handle::current().traced_handle(),
        }
    }

    /// Inert handle: [`spawn`](Self::spawn) falls back to [`tokio::spawn`]
    /// without wake tracking.
    pub fn disabled() -> Self {
        Self {
            runtime: None,
            traced: None,
        }
    }

    /// Handle bound to a specific runtime. Used by the guard's runtime builder
    /// and [`TelemetryGuard::tokio_handle`](super::guard::TelemetryGuard::tokio_handle).
    pub(super) fn for_runtime(
        runtime: tokio::runtime::Handle,
        traced: Option<crate::traced::TracedHandle>,
    ) -> Self {
        Self {
            runtime: Some(runtime),
            traced,
        }
    }

    /// Spawn an instrumented future.
    ///
    /// On an enabled handle the future is wrapped with wake-event tracking. The
    /// task runs on this handle's runtime, the current one for [`current`](Self::current),
    /// or the specific runtime the handle was built for.
    ///
    /// # Panics
    ///
    /// For a [`current`](Self::current)-runtime handle, panics if called outside
    /// a tokio runtime context (same as [`tokio::spawn`]).
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match &self.traced {
            Some(traced) => {
                let _guard = InstrumentedSpawnGuard::enter();
                let future = TracedFuture::new(future, Some(traced.clone()));
                match &self.runtime {
                    Some(rt) => rt.spawn(future),
                    None => tokio::spawn(future),
                }
            }
            None => match &self.runtime {
                Some(rt) => rt.spawn(future),
                None => tokio::spawn(future),
            },
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
    /// # use dial9_tokio_telemetry::telemetry::Dial9TokioHandle;
    /// # use tokio::task::JoinSet;
    /// # async fn work() {}
    /// # async fn demo() {
    /// let handle = Dial9TokioHandle::current();
    /// let mut set: JoinSet<()> = JoinSet::new();
    /// handle.spawn_with(work(), |f| set.spawn(f));
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

/// Spawn a traced task on the current tokio runtime.
///
/// Like [`tokio::spawn`], but wraps the future with wake-event tracking
/// when called from a thread owned by a dial9 runtime. On other threads,
/// falls back to plain [`tokio::spawn`].
///
/// Equivalent to [`Dial9TokioHandle::current().spawn(future)`](Dial9TokioHandle::spawn).
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
    Dial9TokioHandle::current().spawn(future)
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
