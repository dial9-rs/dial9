use crate::TracedFuture;
use dial9_core::handle::Dial9Handle;
use std::cell::Cell;

crate::primitives::thread_local! {
    /// Nest count for [`InstrumentedSpawnGuard`]. `on_task_spawn` treats
    /// any value `> 0` as an instrumented spawn.
    pub(super) static INSTRUMENTED_SPAWN: Cell<u32> = const { Cell::new(0) };
}

/// The handle to instrument with, or `None` when it is not connected to a
/// recorder.
pub(crate) fn traced_handle(handle: &Dial9Handle) -> Option<Dial9Handle> {
    handle.is_connected().then(|| handle.clone())
}

/// Tokio handle for spawning instrumented tasks.
///
/// Spawned futures are wrapped with wake-event tracking when telemetry is live
/// on this handle. Otherwise they spawn plainly. Obtain one for the current
/// runtime with [`current`](Self::current). To spawn onto a specific runtime
/// from any thread, use [`spawn_in`].
///
/// This handle only spawns. For recording and control, use [`Dial9Handle`].
#[derive(Clone)]
pub struct Dial9TokioHandle {
    /// `None` spawns on the current runtime (`tokio::spawn`), `Some` targets a
    /// specific runtime and works from any thread.
    runtime: Option<tokio::runtime::Handle>,
    traced: Option<Dial9Handle>,
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

    /// Handle bound to a specific runtime, so it spawns from any thread.
    #[cfg(test)]
    pub(crate) fn for_runtime(
        runtime: tokio::runtime::Handle,
        traced: Option<Dial9Handle>,
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
    /// For [`tokio::task::JoinSet`], prefer
    /// [`JoinSetExt::spawn_traced`](crate::telemetry::JoinSetExt::spawn_traced).
    ///
    /// # Examples
    ///
    /// Spawn while retaining the task's [`tokio::task::AbortHandle`]:
    ///
    /// ```rust,no_run
    /// # use dial9_tokio_telemetry::telemetry::Dial9TokioHandle;
    /// # async fn work() {}
    /// # async fn demo() {
    /// let handle = Dial9TokioHandle::current();
    /// let abort_handle = handle.spawn_with(work(), |f| tokio::spawn(f).abort_handle());
    /// # }
    /// ```
    ///
    /// The recorded spawn location is the `spawn_fn` call site under
    /// `--cfg tokio_unstable`, and this method's call site without it. An
    /// inline closure like the one above puts both at the same line.
    ///
    /// [`TracedFuture<F>`]: crate::telemetry::TracedFuture
    #[track_caller]
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

    #[track_caller]
    pub(super) fn spawn_in_join_set<F, T>(
        &self,
        set: &mut tokio::task::JoinSet<T>,
        future: F,
    ) -> tokio::task::AbortHandle
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        match &self.traced {
            Some(traced) => {
                let _guard = InstrumentedSpawnGuard::enter();
                set.spawn(TracedFuture::new(future, Some(traced.clone())))
            }
            None => set.spawn(future),
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

/// Spawn a traced task onto a specific runtime, from any thread.
///
/// Like [`tokio::runtime::Handle::spawn`], but the task's polls are instrumented
/// when `runtime` is dial9-traced. Unlike [`spawn`], the calling thread need not
/// belong to a dial9 runtime: instrumentation resolves on the target runtime's
/// worker at the task's first poll. Spawning into an untraced runtime records
/// nothing, the same as [`spawn`] on an untraced runtime.
#[track_caller]
pub fn spawn_in<F>(
    runtime: &tokio::runtime::Handle,
    future: F,
) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    // The spawn hook fires synchronously on this thread, so the guard marks the
    // task instrumented; the future resolves its handle lazily at first poll.
    let _guard = InstrumentedSpawnGuard::enter();
    runtime.spawn(TracedFuture::new_lazy(future))
}

/// Run `future` to completion on `runtime`, instrumented.
///
/// [`Runtime::block_on`](tokio::runtime::Runtime::block_on) polls the future on
/// the calling thread, outside any task, so its polls and wakes are not
/// recorded. This spawns it through [`spawn_in`] instead and waits, so the root
/// future shows up in the trace like any other task.
///
/// # Panics
///
/// Resumes the future's panic on the calling thread, matching
/// [`Runtime::block_on`](tokio::runtime::Runtime::block_on).
#[track_caller]
pub fn block_on<F>(runtime: &tokio::runtime::Runtime, future: F) -> F::Output
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let join = spawn_in(runtime.handle(), future);
    runtime.block_on(async move {
        match join.await {
            Ok(output) => output,
            Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
            Err(_) => unreachable!("task cannot be cancelled inside block_on"),
        }
    })
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
