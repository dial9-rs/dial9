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
//! If the current poll returns `Pending`, a fresh capture is taken via
//! [`tokio::runtime::dump::trace_with`] so that the next poll's sampling
//! decision has fresh data. The capture runs a second `poll` of the inner
//! future under the real waker inside `trace_with`. This may produce a
//! spurious wake (the inner future re-registers the waker, which fires
//! immediately), but avoids lost wakes that would cause task hangs.
//!
//! # Allocation
//!
//! Captured instruction pointers are stored flat in [`FrameBuf`] across all
//! yield points hit during a capture, with offsets recording each callchain's
//! start. The buffers are reused across polls.

use crate::sampling::SplitMix64;
use crate::telemetry::format::TaskDumpEvent;
use crate::telemetry::recorder::SharedState;
use crate::telemetry::task_dump_config::TaskDumpConfig;
use crate::telemetry::task_metadata::TaskId;
use crate::telemetry::{Encodable, ThreadLocalEncoder};
use pin_project_lite::pin_project;
use smallvec::SmallVec;
use std::cell::Cell;
use std::future::Future;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Initial heap reservation for the instruction-pointer buffer on first capture.
const FRAME_BUF_INITIAL_CAPACITY: usize = 256;

crate::primitives::thread_local! {
    /// This session's task-dump config for the current thread. Installed on
    /// every runtime-owned thread: worker thread-start, plus the block_on
    /// thread in `attach_runtime` (which thread-start doesn't fire for on a
    /// current-thread runtime), and cleared on thread stop.
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
        shared: Arc<SharedState>,
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
        // Set after `capture()` re-polls the inner future with the real waker.
        // The re-poll causes a spurious immediate wake; this flag suppresses
        // the next capture to break the busy loop (capture → wake → poll →
        // capture → …). Cleared on the next poll so subsequent real wakes
        // proceed normally.
        just_captured: bool,
        // Whether task dumps are configured for this session. `None` until the
        // first poll reads the per-thread config. The wrapping thread may lack
        // it (e.g. an explicit handle spawned from elsewhere), but the polling
        // thread always has it. `Some(false)` makes poll a passthrough.
        enabled: Option<bool>,
    }
}

impl<F> TaskDumped<F> {
    pub(crate) fn new(inner: F, shared: Arc<SharedState>, task_id: TaskId) -> Self {
        // Config is read lazily on the first poll.
        Self {
            inner,
            shared,
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

        // Read this session's task-dump config on the first poll. Wrapping can
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
        if !enabled || !this.shared.is_enabled() {
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
            this.frames.emit(this.shared, *this.task_id, ts);
            *this.next_sample_ns = this.rng.draw_exponential(*this.sample_mean_ns) as i64;
        }
        match &result {
            Poll::Ready(_) => {
                this.frames.clear();
                *this.pending_capture_ts = None;
            }
            Poll::Pending => {
                // Skip capture if this poll was triggered by the spurious wake
                // from the previous capture's re-poll. This breaks the busy
                // loop: capture → wake → poll → capture → …
                if *this.just_captured {
                    *this.just_captured = false;
                } else {
                    let repoll_result = this.frames.capture(this.inner.as_mut(), cx);
                    // In rare circumstances, repoll will now be ready.
                    if repoll_result.is_ready() {
                        this.frames.clear();
                        *this.pending_capture_ts = None;
                        return repoll_result;
                    }
                    *this.just_captured = true;
                    let poll_end = crate::telemetry::recorder::poll_start_ts_monotonic();
                    *this.pending_capture_ts = NonZeroU64::new(poll_end);
                }
            }
        }
        result
    }
}

/// Metadata for one captured callchain stored in [`FrameBuf`].
struct ChainMeta {
    /// Index into `FrameBuf::ips` where this chain's frames start.
    ip_start: usize,
    /// Address of the root function (upper trim boundary). `None` means trim
    /// to the end of the buffer.
    root_addr: Option<*const core::ffi::c_void>,
    /// Address of the leaf function (lower trim boundary). `None` means no
    /// leaf boundary was available; the chain will be skipped at emit time.
    leaf_addr: Option<*const core::ffi::c_void>,
}

// SAFETY: raw pointers are only used for address comparison, never dereferenced
// across threads.
unsafe impl Send for ChainMeta {}

/// Reusable storage for one or more callchains captured during a single
/// `trace_with` sub-poll. Frames are appended flat to `ips`; each new chain's
/// metadata is pushed onto `chains`.
struct FrameBuf {
    ips: Vec<u64>,
    chains: SmallVec<[ChainMeta; 4]>,
}

impl FrameBuf {
    fn new() -> Self {
        Self {
            ips: Vec::new(),
            chains: SmallVec::new(),
        }
    }

    fn clear(&mut self) {
        self.ips.clear();
        self.chains.clear();
    }

    fn has_data(&self) -> bool {
        !self.chains.is_empty()
    }

    /// Emit one `TaskDumpEvent` per recorded callchain, then clear.
    /// Trimming via `_Unwind_FindEnclosingFunction` happens here (emit path)
    /// rather than during capture, keeping the hot path lock-free.
    fn emit(&mut self, shared: &SharedState, task_id: TaskId, capture_ts: u64) {
        shared.if_enabled(|buf| {
            for (i, meta) in self.chains.iter().enumerate() {
                let ip_end = self
                    .chains
                    .get(i + 1)
                    .map(|next| next.ip_start)
                    .unwrap_or(self.ips.len());
                let raw = &self.ips[meta.ip_start..ip_end];
                let chain = match meta.leaf_addr {
                    Some(leaf) => crate::unwind::trim_frames(raw, meta.root_addr, leaf),
                    None => &[],
                };
                if !chain.is_empty() {
                    buf.record_encodable_event(&TaskDumpData {
                        timestamp_ns: capture_ts,
                        task_id,
                        callchain: chain,
                    });
                }
            }
        });
        self.clear();
    }

    /// Capture backtraces at yield points by re-polling `inner` under the
    /// real waker inside `trace_with`, returning that re-poll's result.
    ///
    /// The re-poll can complete `inner`; that `Ready` is returned so the caller
    /// can adopt it.
    fn capture<F: Future>(&mut self, inner: Pin<&mut F>, cx: &mut Context<'_>) -> Poll<F::Output> {
        if self.ips.capacity() == 0 {
            self.ips.reserve(FRAME_BUF_INITIAL_CAPACITY);
        }
        self.clear();

        let ips = &mut self.ips;
        let chains = &mut self.chains;

        // `trace_with`'s outer closure is `FnOnce`; `Option::take` moves the
        // pinned reference in without requiring a `Copy` bound or unsafe.
        let mut result = Poll::Pending;
        tokio::runtime::dump::trace_with(
            || {
                result = inner.poll(cx);
            },
            |meta| {
                let ip_start = ips.len();
                // Hot path: collect raw IPs only — no _Unwind_FindEnclosingFunction,
                // no dl_iterate_phdr, no global locks. Trimming to root/leaf
                // boundaries happens later in emit().
                crate::unwind::collect_frames_raw(ips);
                // Stash the root/leaf addresses so we can trim at emit time.
                chains.push(ChainMeta {
                    ip_start,
                    root_addr: meta.root_addr,
                    leaf_addr: Some(meta.trace_leaf_addr),
                });
            },
        );
        result
    }
}

/// Borrowed-callchain view of a task-dump event that implements [`Encodable`]
/// by interning its ips into the batch's stack pool.
pub(crate) struct TaskDumpData<'a> {
    pub(crate) timestamp_ns: u64,
    pub(crate) task_id: TaskId,
    pub(crate) callchain: &'a [u64],
}

impl Encodable for TaskDumpData<'_> {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let interned_callchain = enc.intern_stack_frames(self.callchain);
        enc.encode(&TaskDumpEvent {
            timestamp_ns: self.timestamp_ns,
            task_id: self.task_id,
            callchain: interned_callchain,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::TaskDumpData;
    use crate::telemetry::analysis_events::Dial9Event;
    use crate::telemetry::buffer::encode_single;
    use crate::telemetry::format::decode_events;
    use crate::telemetry::task_metadata::TaskId;

    #[test]
    fn task_dump_event_round_trips() {
        let dump = TaskDumpData {
            timestamp_ns: 42_000,
            task_id: TaskId::from_u32(17),
            callchain: &[0x1111_2222, 0x3333_4444, 0x5555_6666],
        };
        let encoded = encode_single(&dump);
        let events = decode_events(&encoded).expect("decode");
        assert_eq!(events.len(), 1);
        let Dial9Event::TaskDumpEvent(ref e) = events[0] else {
            panic!("expected TaskDumpEvent, got {:?}", events[0]);
        };
        assert_eq!(e.timestamp_ns, 42_000);
        assert_eq!(e.task_id, 17);
        assert_eq!(e.callchain, vec![0x1111_2222, 0x3333_4444, 0x5555_6666]);
    }
}
