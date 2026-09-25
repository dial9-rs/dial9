//! `TaskSampled<F>` wraps a future and captures async backtraces at yield
//! points selected by a worker-local capture budget.
//!
//! This wrapper is intentionally separate from the wake-event wrapper: wake
//! capture runs on every instrumented spawn regardless of the `taskdump`
//! feature, while task-dump capture is gated behind the `taskdump` feature and
//! its own runtime toggle. Typical stacking is `WakeTraced<TaskSampled<F>>`.
//!
//! # Sampling model
//!
//! All instrumented tasks on a worker share a calibrated Bernoulli sampler.
//! Non-selected transitions do no capture work. Selected pending captures emit
//! every usable callchain immediately, with the probability used for selection.
//!
//! # Capture mechanics
//!
//! After a normal poll returns `Pending`, capture runs a second `poll` of the
//! inner future under the real waker inside [`tokio::runtime::dump::trace_with`].
//! Tokio may defer a wake for each captured leaf. To avoid a wake-and-capture
//! loop, the poll immediately following a capture polls the future normally
//! but skips capture.
//! On Tokio versions without capture-induced wakes, this also skips capture
//! on the next real wake; it never skips the normal poll.
//!
//! # Allocation
//!
//! Captured instruction pointers are stored flat in [`FrameBuf`] across all
//! yield points hit during a capture, with offsets recording each callchain's
//! start. The buffers are reused across polls.

use super::capture::FrameBuf;
use crate::primitives::sync::Arc;
use crate::task_dump::worker::WorkerSampler;
use crate::telemetry::task_metadata::TaskId;
use dial9_core::handle::Dial9Handle;
use pin_project_lite::pin_project;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

crate::primitives::thread_local! {
    // Cache the current worker's shared state. RuntimeContext owns its lifetime,
    // so switching runtimes or moving a worker to another thread preserves it.
    static SAMPLER: RefCell<Option<Arc<WorkerSampler>>> = const { RefCell::new(None) };
}

/// Install the sampler when a runtime hook resolves this worker's identity.
pub(crate) fn set_worker_sampler(sampler: Option<Arc<WorkerSampler>>) {
    SAMPLER.with(|cell| *cell.borrow_mut() = sampler);
}

pub(crate) fn clear_worker_sampler() {
    set_worker_sampler(None);
}

fn refresh_worker_sampler(cached: &mut Option<Arc<WorkerSampler>>) {
    SAMPLER.with(|cell| {
        let current = cell.borrow();
        if current.as_ref().map(Arc::as_ptr) != cached.as_ref().map(Arc::as_ptr) {
            *cached = current.clone();
        }
    });
}

// ─── TaskSampled future wrapper ──────────────────────────────────────────────

pin_project! {
    /// Future wrapper that captures async backtraces at yield points using
    /// a shared worker-local capture budget.
    pub(crate) struct TaskSampled<F> {
        #[pin]
        inner: F,
        handle: Dial9Handle,
        task_id: TaskId,
        frames: FrameBuf,
        // Retain the poll's worker across nested runtimes that replace TLS.
        // Refresh only on migration, avoiding an Arc clone on every poll.
        sampler: Option<Arc<WorkerSampler>>,
        // Skip the next capture, but not the normal poll, to break capture-wake loops.
        just_captured: bool,
    }
}

impl<F> TaskSampled<F> {
    pub(crate) fn new(inner: F, handle: Dial9Handle, task_id: TaskId) -> Self {
        Self {
            inner,
            handle,
            task_id,
            frames: FrameBuf::new(),
            sampler: None,
            just_captured: false,
        }
    }
}

impl<F: Future> Future for TaskSampled<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let mut this = self.project();
        refresh_worker_sampler(this.sampler);
        let Some(sampler) = this.sampler.as_ref() else {
            *this.just_captured = false;
            return this.inner.poll(cx);
        };
        if !this.handle.is_enabled() {
            *this.just_captured = false;
            return this.inner.poll(cx);
        }
        let result = this.inner.as_mut().poll(cx);
        if result.is_ready() {
            return result;
        }
        if std::mem::take(this.just_captured) {
            return Poll::Pending;
        }
        let Some(probability) =
            sampler.observe_pending(crate::telemetry::events::clock_monotonic_ns)
        else {
            return Poll::Pending;
        };
        let result = this.frames.capture(this.inner.as_mut(), cx);
        if result.is_ready() {
            // A completed capture re-poll has no following idle interval.
            this.frames.clear();
            return result;
        }
        *this.just_captured = true;
        // Read the actual capture time once for the entire callchain group.
        // PollStart can precede capture by substantial application work.
        let timestamp = crate::telemetry::events::clock_monotonic_ns();
        this.frames
            .emit_sample(this.handle, *this.task_id, timestamp, probability);
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::super::capture::TaskSampleData;
    use super::*;
    use crate::primitives::sync::atomic::AtomicU64;
    use crate::telemetry::TaskSamplingConfig;
    use crate::telemetry::analysis_events::Dial9Event;
    use crate::telemetry::encoder::encode_single;
    use crate::telemetry::format::decode_events;
    use crate::telemetry::task_metadata::TaskId;

    fn install_test_sampler(calibrated: bool) {
        clear_worker_sampler();
        let config = TaskSamplingConfig::builder().rng_seed(42).build();
        let now = crate::telemetry::events::clock_monotonic_ns();
        let start = if calibrated { now - 1_000_000_000 } else { now };
        set_worker_sampler(Some(Arc::new(WorkerSampler::new(
            config,
            0,
            start,
            Arc::new(AtomicU64::new(0)),
        ))));
    }

    #[test]
    fn non_selected_polls_never_repoll_or_reserve_frames() {
        use std::cell::Cell;
        use std::future::poll_fn;
        use std::task::Waker;

        let recorder =
            crate::telemetry::recorder(crate::telemetry::MemoryBuffer::new(1024 * 1024).unwrap())
                .build();
        install_test_sampler(false);
        let polls = Cell::new(0);
        let inner = poll_fn(|_| {
            polls.set(polls.get() + 1);
            Poll::<()>::Pending
        });
        let mut future = TaskSampled::new(inner, recorder.handle().clone(), TaskId::from_u32(1));
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..100 {
            assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        }
        assert_eq!(polls.get(), 100);
        assert_eq!(future.frames.capacity(), 0);

        // Exercise the calibrated geometric-skip path as well as warm-up.
        SAMPLER.with(|cell| {
            let now = crate::telemetry::events::clock_monotonic_ns();
            let sampler = WorkerSampler::new(
                TaskSamplingConfig::builder()
                    .captures_per_second_per_worker(1)
                    .rng_seed(42)
                    .build(),
                0,
                now - 1_000_000_000,
                Arc::new(AtomicU64::new(0)),
            );
            for i in 0..100_000 {
                assert_eq!(sampler.observe_pending(|| now - 1_000_000_000 + i), None);
            }
            *cell.borrow_mut() = Some(Arc::new(sampler));
        });
        for _ in 0..100 {
            assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        }
        assert_eq!(polls.get(), 200);
        assert_eq!(future.frames.capacity(), 0);

        install_test_sampler(true);
        recorder.disable();
        assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        assert_eq!(polls.get(), 201);
        assert_eq!(future.frames.capacity(), 0);
        clear_worker_sampler();
    }

    #[test]
    fn ready_during_selected_repoll_is_returned_without_emission() {
        use std::future::poll_fn;
        use std::task::Waker;

        let recorder =
            crate::telemetry::recorder(crate::telemetry::MemoryBuffer::new(1024 * 1024).unwrap())
                .build();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        install_test_sampler(true);
        let mut polls = 0;
        let inner = poll_fn(|_| {
            polls += 1;
            match polls {
                1 => Poll::Pending,
                2 => Poll::Ready(17),
                _ => panic!("polled after completion"),
            }
        });
        let mut future = TaskSampled::new(inner, recorder.handle().clone(), TaskId::from_u32(1));
        assert_eq!(
            Pin::new(&mut future).poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(17)
        );
        assert!(future.frames.is_empty());
        assert!(!future.just_captured);
        clear_worker_sampler();
    }

    #[test]
    fn task_dump_event_round_trips() {
        let dump = TaskSampleData {
            timestamp_ns: 42_000,
            task_id: TaskId::from_u32(17),
            callchain: &[0x1111_2222, 0x3333_4444, 0x5555_6666],
            inclusion_probability: 0.125,
        };
        let encoded = encode_single(&dump);
        let events = decode_events(&encoded).expect("decode");
        assert_eq!(events.len(), 1);
        let Dial9Event::TaskSampleEvent(ref e) = events[0] else {
            panic!("expected TaskSampleEvent, got {:?}", events[0]);
        };
        assert_eq!(e.timestamp_ns, 42_000);
        assert_eq!(e.task_id, 17);
        assert_eq!(e.inclusion_probability, 0.125);
        assert_eq!(e.callchain, vec![0x1111_2222, 0x3333_4444, 0x5555_6666]);
    }
}
