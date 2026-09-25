//! `TaskDumped<F>` wraps a future and captures async backtraces at yield
//! points using Poisson sampling keyed on idle duration.
//!
//! This wrapper is intentionally separate from the wake-event wrapper: wake
//! capture runs on every instrumented spawn regardless of the `taskdump`
//! feature, while task-dump capture is gated behind the `taskdump` feature and
//! its own runtime toggle. Typical stacking is `WakeTraced<TaskDumped<F>>`.
//!
//! # Sampling model
//!
//! Instead of a hard time cutoff, each task maintains a byte-counter–style
//! `next_sample_ns` drawn from an exponential distribution with mean equal to
//! the configured `idle_threshold`. On each poll, the preceding idle duration
//! is subtracted from the counter. When the counter reaches zero or below, the
//! captured frames are emitted and a new gap is drawn. This gives unbiased
//! Poisson sampling: longer idles are more likely to trigger a dump, but even
//! short idles have a non-zero (if small) probability.
//!
//! # Capture mechanics
//!
//! After a normal poll returns `Pending`, capture runs a second `poll` of the
//! inner future under the real waker inside [`tokio::runtime::dump::trace_with`].
//! Tokio may defer a wake for each captured leaf. To avoid a wake-and-capture
//! loop, the poll immediately following a capture polls the future normally
//! but skips capture, retaining any un-emitted frames and their timestamp.
//! On Tokio versions without capture-induced wakes, this also skips capture
//! on the next real wake; it never skips the normal poll.
//!
//! # Allocation
//!
//! Captured instruction pointers are stored flat in [`FrameBuf`] across all
//! yield points hit during a capture, with offsets recording each callchain's
//! start. The buffers are reused across polls.

use super::capture::FrameBuf;
use crate::sampling::SplitMix64;
use crate::telemetry::task_dump_config::TaskDumpConfig;
use crate::telemetry::task_metadata::TaskId;
use dial9_core::handle::Dial9Handle;
use pin_project_lite::pin_project;
use std::cell::Cell;
use std::future::Future;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::task::{Context, Poll};

crate::primitives::thread_local! {
    /// This recorder's task-dump config for the current thread. Installed on
    /// every runtime-owned thread: worker thread-start, plus the block_on
    /// thread in `attach_tokio_runtime` (which thread-start doesn't fire for on a
    /// current-thread runtime), and cleared on thread stop.
    /// Refreshed before each task poll to follow runtime changes.
    /// `None` means task dumps aren't configured, so `TaskDumped` runs as a passthrough.
    static TASKDUMP_CONFIG: Cell<Option<TaskDumpConfig>> = const { Cell::new(None) };
}

/// Install task-dump config for the current thread (runtime thread-start hook).
pub(crate) fn set_taskdump_config(config: TaskDumpConfig) {
    TASKDUMP_CONFIG.with(|c| c.set(Some(config)));
}

/// Clear the current thread's task-dump config (runtime thread-stop hook).
pub(crate) fn clear_taskdump_config() {
    TASKDUMP_CONFIG.with(|c| c.set(None));
}

// ─── TaskDumped future wrapper ──────────────────────────────────────────────

pin_project! {
    /// Future wrapper that captures async backtraces at yield points using
    /// Poisson sampling keyed on idle duration.
    pub(crate) struct TaskDumped<F> {
        #[pin]
        inner: F,
        handle: Dial9Handle,
        task_id: TaskId,
        frames: FrameBuf,
        // Monotonic nanoseconds when the frames in `frames` were captured.
        // Only meaningful when `frames.has_data()`.
        pending_capture_ts: Option<NonZeroU64>,
        // Sampling state: remaining nanoseconds of idle time before
        // the next sample triggers. Signed so subtracting a large idle from a
        // small remaining value goes negative rather than wrapping.
        next_sample_ns: i64,
        // Mean of the exponential distribution (nanoseconds).
        sample_mean_ns: u64,
        // Per-task PRNG for drawing exponential gaps.
        rng: SplitMix64,
        // Skip the next capture, but not the normal poll, to break capture-wake loops.
        just_captured: bool,
        // Whether task dumps are configured for this recorder. `None` until the
        // first poll reads the per-thread config. The wrapping thread may lack
        // it (e.g. an explicit handle spawned from elsewhere), but the polling
        // thread always has it. `Some(false)` makes poll a passthrough.
        enabled: Option<bool>,
    }
}

impl<F> TaskDumped<F> {
    pub(crate) fn new(inner: F, handle: Dial9Handle, task_id: TaskId) -> Self {
        // Config is read lazily on the first poll.
        Self {
            inner,
            handle,
            task_id,
            frames: FrameBuf::new(),
            pending_capture_ts: None,
            next_sample_ns: 0,
            sample_mean_ns: 0,
            rng: SplitMix64::new(0),
            just_captured: false,
            enabled: None,
        }
    }
}

impl<F: Future> Future for TaskDumped<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let mut this = self.project();

        // Read this recorder's task-dump config on the first poll. Wrapping can
        // happen on a thread without the config, but a task always polls on a
        // runtime-owned thread, which has it.
        let enabled = match *this.enabled {
            Some(e) => e,
            None => {
                let config = TASKDUMP_CONFIG.with(|c| c.get());
                if let Some(cfg) = config {
                    *this.sample_mean_ns = cfg.idle_threshold().as_nanos() as u64;
                    // Fixed seed for deterministic tests; otherwise derive from
                    // task_id + time for production uniqueness.
                    let seed = cfg.rng_seed().unwrap_or_else(|| {
                        this.task_id.to_u64().wrapping_mul(0x517cc1b727220a95)
                            ^ crate::telemetry::events::clock_monotonic_ns()
                    });
                    *this.rng = SplitMix64::new(seed);
                    *this.next_sample_ns = this.rng.draw_exponential(*this.sample_mean_ns) as i64;
                }
                let e = config.is_some();
                *this.enabled = Some(e);
                e
            }
        };

        // Fast path: forward without any capture work when either task dumps
        // are disabled, or telemetry as a whole is paused.
        if !enabled || !this.handle.is_enabled() {
            *this.just_captured = false;
            if this.frames.has_data() {
                this.frames.clear();
                *this.pending_capture_ts = None;
            }
            return this.inner.poll(cx);
        }
        // Poisson sampling over idle time: subtract the idle duration from
        // the counter. If it goes to zero or below, emit and redraw a fresh
        // interval. Short idles have a small but nonzero chance of being
        // sampled (~ idle / mean); long idles are sampled with probability
        // approaching 1. At most one emission per poll.
        let poll_start = crate::telemetry::recorder::poll_start_ts_monotonic();
        let should_emit = match *this.pending_capture_ts {
            Some(ts) if this.frames.has_data() => {
                let idle_ns = poll_start.saturating_sub(ts.get()) as i64;
                *this.next_sample_ns -= idle_ns;
                *this.next_sample_ns <= 0
            }
            _ => false,
        };
        let result = this.inner.as_mut().poll(cx);
        if should_emit {
            let ts = this
                .pending_capture_ts
                .expect("checked in match above")
                .get();
            this.frames.emit(this.handle, *this.task_id, ts);
            *this.next_sample_ns = this.rng.draw_exponential(*this.sample_mean_ns) as i64;
        }
        match &result {
            Poll::Ready(_) => {
                this.frames.clear();
                *this.pending_capture_ts = None;
            }
            Poll::Pending => {
                if std::mem::take(this.just_captured) {
                    return Poll::Pending;
                }
                let repoll_result = this.frames.capture(this.inner.as_mut(), cx);
                // In rare circumstances, repoll will now be ready.
                if repoll_result.is_ready() {
                    this.frames.clear();
                    *this.pending_capture_ts = None;
                    return repoll_result;
                }
                *this.just_captured = true;
                let capture_ts = crate::telemetry::recorder::poll_start_ts_monotonic();
                *this.pending_capture_ts = NonZeroU64::new(capture_ts);
            }
        }
        result
    }
}
