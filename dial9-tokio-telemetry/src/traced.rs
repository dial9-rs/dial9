//! Future wrappers for task instrumentation.

use crate::rate_limit::rate_limited;
use crate::telemetry::format::WakeEventEvent;
use crate::telemetry::recorder::SharedState;
use crate::telemetry::task_metadata::TaskId;
use dial9_core::handle::Dial9Handle;
use futures_util::task::{ArcWake, AtomicWaker, waker as arc_waker};
use pin_project_lite::pin_project;
use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// Handle used by instrumented futures to emit events into the telemetry system.
#[derive(Clone)]
pub(crate) struct TracedHandle {
    pub(crate) shared: Arc<SharedState>,
}

impl std::fmt::Debug for TracedHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracedHandle").finish_non_exhaustive()
    }
}

#[cfg(feature = "taskdump")]
type MaybeTaskDumped<F> = crate::task_dumped::TaskDumped<F>;

#[cfg(not(feature = "taskdump"))]
type MaybeTaskDumped<F> = F;

type InstrumentedFuture<F> = WakeTraced<MaybeTaskDumped<F>>;

pin_project! {
    /// Future wrapper produced by `spawn_with` for custom spawn APIs.
    ///
    /// On first poll, `TracedFuture<F>` resolves the surrounding Tokio task ID
    /// and uses it for task instrumentation. If the future is polled outside a
    /// Tokio task context, it runs as a transparent passthrough without wake
    /// tracking or task dumps.
    pub struct TracedFuture<F> {
        #[pin]
        state: TracedFutureState<F>,
        // Carried to the first poll, where the task id becomes resolvable.
        spawn_loc: &'static Location<'static>,
    }
}

impl<F> std::fmt::Debug for TracedFuture<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracedFuture").finish_non_exhaustive()
    }
}

/// How a [`TracedFuture`] obtains its recording handle at first poll.
enum HandleSource {
    /// Instrument with this handle when a Tokio task context is present.
    /// Captured eagerly at spawn time on the spawning thread.
    Eager(TracedHandle),
    /// Never instrument, run as a transparent passthrough.
    Passthrough,
    /// Resolve the handle from the polling thread's current dial9 runtime at
    /// first poll. Used by [`spawn_in`](crate::telemetry::spawn_in), which
    /// spawns from an arbitrary thread where the target runtime's handle is not
    /// yet reachable; the first poll runs on that runtime's worker, where the
    /// thread-local handle is set.
    Lazy,
}

impl<F> TracedFuture<F> {
    #[track_caller]
    pub(crate) fn new(inner: F, handle: Option<TracedHandle>) -> Self {
        let source = match handle {
            Some(handle) => HandleSource::Eager(handle),
            None => HandleSource::Passthrough,
        };
        Self {
            // The spawn site, resolved through the `#[track_caller]` chain from
            // the public spawn helpers. Stands in for `TaskMeta::spawned_at()`.
            spawn_loc: Location::caller(),
            state: TracedFutureState::Init { inner, source },
        }
    }

    /// Defer handle resolution to the first poll (see [`HandleSource::Lazy`]).
    #[track_caller]
    pub(crate) fn new_lazy(inner: F) -> Self {
        Self {
            spawn_loc: Location::caller(),
            state: TracedFutureState::Init {
                inner,
                source: HandleSource::Lazy,
            },
        }
    }
}

pin_project! {
    #[project = TracedFutureStateProj]
    #[project_replace = TracedFutureStateProjReplace]
    enum TracedFutureState<F> {
        Init {
            inner: F,
            source: HandleSource,
        },
        Passthrough {
            #[pin]
            inner: F,
        },
        Instrumented {
            #[pin]
            inner: InstrumentedFuture<F>,
        },
        Empty,
    }
}

pin_project! {
    /// Future wrapper that captures wake events for a known Tokio task.
    pub(crate) struct WakeTraced<F> {
        #[pin]
        inner: F,
        waker_data: Arc<TracedWakerData>, // reused across polls to avoid a per-poll Arc allocation
        // Spawn site for the poll events this wrapper emits in place of
        // tokio's poll hooks (`pin_project!` takes no `#[cfg]` on fields, so
        // it is carried unconditionally).
        spawn_loc: &'static Location<'static>,
    }
}

impl<F> WakeTraced<F> {
    pub(crate) fn new(
        inner: F,
        handle: TracedHandle,
        task_id: TaskId,
        spawn_loc: &'static Location<'static>,
    ) -> Self {
        let waker_data = Arc::new(TracedWakerData {
            inner: AtomicWaker::new(),
            woken_task_id: task_id,
            shared: handle.shared.clone(),
        });
        Self {
            inner,
            waker_data,
            spawn_loc,
        }
    }
}

#[cfg(not(tokio_unstable))]
thread_local! {
    /// Poll nesting depth for this thread.
    ///
    /// A `TracedFuture` awaited inside another one's poll would otherwise emit
    /// a span nested inside its parent's, which the viewer's flat per-worker
    /// span model cannot represent. Only the outermost wrapper emits.
    static POLL_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

// --- Waker wrapping ---

/// Shared state threaded through our custom `Waker`.
///
/// `inner` is an `AtomicWaker` so that the waker registered by the executor
/// can be stored and replaced in a thread-safe way without allocating a new
/// `Arc` on every `poll`.
struct TracedWakerData {
    inner: AtomicWaker,
    woken_task_id: TaskId,
    shared: Arc<SharedState>,
}

impl ArcWake for TracedWakerData {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        record_wake_event(arc_self);
        arc_self.inner.wake();
    }
}

fn record_wake_event(data: &TracedWakerData) {
    data.shared.if_enabled(|buf| {
        // The worker issuing the wake — not the worker that will execute the woken task
        // (which is unknowable at wake time). Stored in the event as `target_worker`.
        let waking_worker_id = crate::telemetry::recorder::current_worker_id();
        // TODO: cleanly handle more than 255 global workers in the wake event wire format.
        // Wake event wire format uses u8; clamp large worker IDs to UNKNOWN (255).
        let waking_worker_u8 = if waking_worker_id.as_u64() <= 254 {
            waking_worker_id.as_u64() as u8
        } else {
            255
        };
        let waker_task_id = tokio::task::try_id().map(TaskId::from).unwrap_or_default();
        let event = WakeEventEvent {
            timestamp_ns: crate::telemetry::events::clock_monotonic_ns(),
            waker_task_id,
            woken_task_id: data.woken_task_id,
            target_worker: waking_worker_u8,
        };
        buf.record_encodable_event(&event);
    });
}

fn make_traced_waker(data: Arc<TracedWakerData>) -> Waker {
    arc_waker(data)
}

fn make_instrumented<F>(
    inner: F,
    handle: TracedHandle,
    task_id: TaskId,
    spawn_loc: &'static std::panic::Location<'static>,
) -> InstrumentedFuture<F>
where
    F: Future,
{
    let shared = handle.shared.clone();
    #[cfg(feature = "taskdump")]
    let inner = crate::task_dumped::TaskDumped::new(inner, shared, task_id);
    #[cfg(not(feature = "taskdump"))]
    let _ = shared;

    WakeTraced::new(inner, handle, task_id, spawn_loc)
}

impl<F: Future> Future for TracedFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let spawn_loc = *this.spawn_loc;
        let mut state = this.state;

        loop {
            match state.as_mut().project() {
                TracedFutureStateProj::Init { .. } => {}
                TracedFutureStateProj::Passthrough { inner } => return inner.poll(cx),
                TracedFutureStateProj::Instrumented { inner } => return inner.poll(cx),
                TracedFutureStateProj::Empty => {
                    unreachable!("TracedFutureState::Empty is only used during state transitions")
                }
            }

            let TracedFutureStateProjReplace::Init { inner, source } =
                state.as_mut().project_replace(TracedFutureState::Empty)
            else {
                unreachable!("Init was matched immediately before project_replace")
            };

            // Resolve the recording handle. `Lazy` reads the polling thread's
            // current dial9 runtime (set on the worker's thread-local), so a task
            // spawned from off-runtime via `spawn_in` still instruments here.
            let handle = match source {
                HandleSource::Eager(handle) => Some(handle),
                HandleSource::Passthrough => None,
                HandleSource::Lazy => {
                    crate::telemetry::recorder::traced_handle(&Dial9Handle::current())
                }
            };

            let Some(handle) = handle else {
                state.set(TracedFutureState::Passthrough { inner });
                continue;
            };

            let Some(task_id) = tokio::task::try_id().map(TaskId::from) else {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        "Traced future polled outside a Tokio task context; running future without instrumentation"
                    );
                });
                state.set(TracedFutureState::Passthrough { inner });
                continue;
            };

            let inner = make_instrumented(inner, handle, task_id, spawn_loc);
            state.set(TracedFutureState::Instrumented { inner });
        }
    }
}

impl<F: Future> Future for WakeTraced<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if !this.waker_data.shared.is_enabled() {
            return this.inner.poll(cx);
        }

        // Store (or replace) the executor's waker so that when our custom
        // waker fires it can forward the notification to the correct waker,
        // even if the task has been moved to a different executor thread
        // between polls.
        this.waker_data.inner.register(cx.waker());

        let traced_waker = make_traced_waker(this.waker_data.clone());
        let mut traced_cx = Context::from_waker(&traced_waker);

        #[cfg(tokio_unstable)]
        {
            this.inner.poll(&mut traced_cx)
        }
        #[cfg(not(tokio_unstable))]
        {
            use crate::telemetry::recorder::make_poll_start;

            let outermost = POLL_DEPTH.with(|d| {
                let n = d.get();
                d.set(n + 1);
                n == 0
            });
            // Restores the depth and closes the span on the way out, panic or
            // not. A panicking future unwinds past the rest of this function and
            // tokio catches it above us.
            let _guard = PollDepthGuard {
                shared: outermost.then(|| &*this.waker_data.shared),
            };
            if outermost {
                this.waker_data.shared.if_enabled(|buf| {
                    buf.record_encodable_event(&make_poll_start(
                        None,
                        &this.waker_data.shared,
                        this.spawn_loc,
                        this.waker_data.woken_task_id,
                    ));
                });
            }
            this.inner.poll(&mut traced_cx)
        }
    }
}

/// Restores this thread's poll depth, and closes the poll span when this is the
/// outermost wrapper. `shared` is `Some` only then, so a nested wrapper unwinds
/// without emitting.
#[cfg(not(tokio_unstable))]
struct PollDepthGuard<'a> {
    shared: Option<&'a SharedState>,
}

#[cfg(not(tokio_unstable))]
impl Drop for PollDepthGuard<'_> {
    fn drop(&mut self) {
        POLL_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        if let Some(shared) = self.shared {
            shared.if_enabled(|buf| {
                buf.record_encodable_event(&crate::telemetry::recorder::make_poll_end(
                    None, shared,
                ));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::analysis_events::Dial9Event;
    use crate::telemetry::buffer::{DiskBuffer, MemoryBuffer};
    use crate::telemetry::recorder::{
        Dial9HandleTokioExt, Dial9TokioHandle, TokioAttachOptions, traced_handle,
    };
    use crate::telemetry::task_metadata::UNKNOWN_TASK_ID;
    use dial9_core::recorder::recorder;
    use dial9_core::test_util;
    use futures_util::task::noop_waker;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::Context;
    use tempfile::TempDir;

    #[test]
    fn traced_future_falls_back_after_missing_task_context() {
        let rec = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap()).build();
        let handle = traced_handle(rec.handle()).expect("enabled handle yields TracedHandle");

        let mut future = TracedFuture::new(std::future::pending::<()>(), Some(handle));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        assert!(matches!(
            future.state,
            TracedFutureState::Passthrough { .. }
        ));

        // This is important to ensure the missing-task fallback is one-way:
        // after the first failed task-id lookup, later polls go straight
        // through without retrying `try_id()` or warning again.
        assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        assert!(matches!(
            future.state,
            TracedFutureState::Passthrough { .. }
        ));
    }

    /// Verify that the wake-tracking wrapper records a `WakeEvent` whose `woken_task_id`
    /// matches the spawned task when a `Notify` wakes it.
    ///
    /// This is an integration test: events are written to a real file via
    /// `DiskBuffer` and then read back with `TraceReader`.
    #[test]
    #[cfg(feature = "analysis")]
    fn traced_emits_wake_events() {
        use crate::telemetry::analysis::TraceReader;
        let dir = TempDir::new().unwrap();
        let trace_path = dir.path().join("trace.bin");

        // Build a current-thread runtime so that all tasks — and all thread-local
        // BUFFER accesses — share a single thread with the test itself.
        let rec = recorder(DiskBuffer::single_file(&trace_path).unwrap()).build();
        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();
        let runtime = rec
            .handle()
            .attach_tokio_runtime(builder, TokioAttachOptions::default())
            .unwrap();
        let handle =
            Dial9TokioHandle::for_runtime(runtime.handle().clone(), traced_handle(rec.handle()));
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = notify.clone();

        // We'll capture the spawned task's ID from inside the task so we can
        // assert the correct `woken_task_id` appears in the recorded events.
        let spawned_id: Arc<Mutex<TaskId>> = Arc::new(Mutex::new(UNKNOWN_TASK_ID));
        let spawned_id_write = spawned_id.clone();

        runtime.block_on(async {
            // Spawn a traced task that blocks on a Notify.
            let join = handle.spawn(async move {
                *spawned_id_write.lock().unwrap() = tokio::task::try_id()
                    .map(TaskId::from)
                    .unwrap_or(UNKNOWN_TASK_ID);
                notify_clone.notified().await;
            });

            // Yield so the spawned task runs its first poll and registers its
            // waker with the Notify before we send the notification.
            tokio::task::yield_now().await;

            // This calls wake_by_ref on our TracedWakerData, recording the WakeEvent.
            notify.notify_one();

            join.await.unwrap();
        });

        // Wake events land in the thread-local buffer (capacity 1_024), so a
        // single event will not auto-flush.  Manually drain the buffer into the
        // collector so that the guard flush below picks it up.
        let th = traced_handle(rec.handle()).expect("enabled handle yields TracedHandle");
        test_util::drain_thread_local(&th.shared);

        // Dropping the runtime + recorder stops the background flush thread, joins
        // it, then performs a final flush: collector → DiskBuffer → trace file.
        drop(runtime);
        drop(rec);

        // Parse the trace file and collect all WakeEvents.
        let sealed = dir.path().join("trace.0.bin");
        let trace_path_str = sealed.to_str().unwrap();
        let reader = TraceReader::new(trace_path_str).unwrap();
        let events = &reader.runtime_events;

        let wake_task_ids: Vec<TaskId> = events
            .iter()
            .filter_map(|e| {
                if let Dial9Event::WakeEvent(w) = e {
                    Some(TaskId(w.woken_task_id))
                } else {
                    None
                }
            })
            .collect();

        assert!(
            !wake_task_ids.is_empty(),
            "expected at least one WakeEvent but got none; all events: {events:#?}"
        );

        let expected = *spawned_id.lock().unwrap();
        assert_ne!(
            expected, UNKNOWN_TASK_ID,
            "spawned task should have a real tokio task ID"
        );
        assert!(
            wake_task_ids.contains(&expected),
            "no WakeEvent with woken_task_id={expected:?}; recorded wake_task_ids={wake_task_ids:?}"
        );
    }
}
