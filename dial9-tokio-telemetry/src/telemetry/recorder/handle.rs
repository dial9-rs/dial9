use crate::TracedFuture;
use crate::traced::TracedHandle;
use dial9_core::handle::Dial9Handle;
use std::cell::Cell;

crate::primitives::thread_local! {
    /// Nest count for [`InstrumentedSpawnGuard`]. `on_task_spawn` treats
    /// any value `> 0` as an instrumented spawn.
    pub(super) static INSTRUMENTED_SPAWN: Cell<u32> = const { Cell::new(0) };
}

/// Wake-tracking handle for a [`Dial9Handle`], or `None` when the handle is
/// disabled. The waker wrapping is tokio-specific, so it lives here rather
/// than on the runtime-agnostic `Dial9Handle`.
pub(crate) fn traced_handle(handle: &Dial9Handle) -> Option<TracedHandle> {
    handle.shared().map(|shared| TracedHandle {
        shared: shared.clone(),
    })
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
            traced: traced_handle(&Dial9Handle::current()),
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
