#![deny(clippy::arithmetic_side_effects)]
//! Allocator hook — the hot path called from `Dial9Allocator::alloc`/etc.
//!
//! # Soundness contract
//!
//! Every function in this module must be **allocator-quiet** — it must not
//! allocate, deallocate, lock a mutex, call `tracing::warn!`, or do
//! anything else that could trigger another allocation while we are
//! inside `GlobalAlloc::alloc`. Any allocation here would re-enter our
//! own hook and either deadlock or recurse without bound.
//!
//! Allowed operations:
//! - `OnceLock::get` (Acquire load + null check, ~1 ns).
//! - TLS access via `RefCell::try_borrow_mut`.
//! - Stack-only `RawAlloc` / `RawFree` construction.
//! - `Unwinder::capture` into a stack-resident `[u64; DEFAULT_MAX_FRAMES]`.
//! - `ArrayQueue::push` (lock-free CAS, allocation-free).
//! - `AtomicU64::fetch_add` (for the dropped-sample counter).
//!
//! Forbidden operations:
//! - Any call that may allocate (`Vec::push`, `HashMap::insert`,
//!   `String::from`, ...).
//! - Any lock acquisition (`Mutex::lock`, `RwLock::read`, ...).
//! - `record_encodable_event` / `record_event` / any encoder access — that
//!   path allocates and is the consolidator's job, not ours.
//! - `tracing::warn!` / `tracing::error!` — formatters allocate.
//! - `current_tid()` IS allowed (it's either a vDSO syscall or a TLS
//!   counter, no allocation).
//!
//! See design §6 (Reentrancy) for the full argument.

use crate::memory_profiling::config::TimestampMode;
use crate::memory_profiling::profiler::MemoryProfilerInner;
use crate::memory_profiling::ring::{DEFAULT_MAX_FRAMES, RawAlloc, RawFree};
use crate::sampling::SplitMix64;
use crate::telemetry::events::current_tid;

/// Per-thread sampling state. Held in TLS so each thread reads/writes
/// its own counter and PRNG without synchronization (design §1).
struct SamplingState {
    /// Remaining bytes until the next sample fires. Signed so subtracting
    /// a large allocation from a small remaining value goes negative
    /// rather than wrapping.
    next_sample_bytes: i64,
    /// Per-thread PRNG. Lazily seeded on first sample.
    rng: SplitMix64,
    /// Whether this thread's state has been initialized.
    initialized: bool,
}

impl SamplingState {
    const fn empty() -> Self {
        Self {
            next_sample_bytes: 0,
            rng: SplitMix64::new(0),
            initialized: false,
        }
    }
}

thread_local! {
    /// Per-thread sampling state, wrapped in `RefCell` so we can detect
    /// re-entrant calls via `try_borrow_mut`. If any code path inside
    /// `on_alloc` somehow allocates while the borrow is held, the
    /// re-entrant call gets `Err(BorrowMutError)` and silently skips
    /// — the design's reentrancy guard (§6).
    static SAMPLE_STATE: std::cell::RefCell<SamplingState> =
        const { std::cell::RefCell::new(SamplingState::empty()) };
}

#[inline]
fn ensure_initialized(state: &mut SamplingState, inner: &MemoryProfilerInner) {
    if state.initialized {
        return;
    }
    let nonce = current_tid() as u64;
    let seed = match inner.rng_seed {
        Some(s) => s.wrapping_add(nonce),
        None => {
            // Mix wall clock with the thread ID and the static address.
            // `nonce` (= current_tid()) guarantees uniqueness across threads
            // even if they initialize at the same nanosecond.
            let now = crate::telemetry::events::clock_monotonic_ns();
            now.wrapping_mul(0x517cc1b727220a95) ^ nonce ^ (inner as *const _ as u64)
        }
    };
    state.rng = SplitMix64::new(seed);
    state.next_sample_bytes =
        i64::try_from(state.rng.draw_exponential(inner.sample_rate_bytes)).unwrap_or(i64::MAX);
    state.initialized = true;
}

#[inline]
fn stamp(mode: TimestampMode) -> u64 {
    match mode {
        TimestampMode::ReusePollStart => crate::telemetry::recorder::poll_start_ts_or_now(),
        TimestampMode::None => 0,
        TimestampMode::Precise => crate::telemetry::events::clock_monotonic_ns(),
    }
}

/// Allocator hook: called from `Dial9Allocator::alloc` after the inner
/// allocation succeeds (`ptr` is non-null).
///
/// SAFETY: must be allocator-quiet — see module docs.
#[inline]
pub(crate) fn on_alloc(inner: &MemoryProfilerInner, ptr: *mut u8, size: usize) {
    // `try_with` returns `Err` if the TLS slot is being destroyed during
    // thread teardown — silently skip those allocations rather than
    // risking UB.
    let _ = SAMPLE_STATE.try_with(|cell| {
        // `try_borrow_mut` is the reentrancy guard: if the current
        // thread is already inside `on_alloc` higher up the stack,
        // the borrow is already held and this call returns `Err`.
        let Ok(mut state) = cell.try_borrow_mut() else {
            return;
        };
        ensure_initialized(&mut state, inner);

        // Saturate at i64::MAX — allocations this large are unrealistic but we
        // handle them defensively rather than wrapping.
        let size_i64 = i64::try_from(size).unwrap_or(i64::MAX);

        // OK: intentionally allowed to go negative — the design uses signed arithmetic
        // so that a large allocation exceeding the remaining budget produces a negative value,
        // which triggers sampling on the next line.
        #[allow(clippy::arithmetic_side_effects)]
        let remaining = state.next_sample_bytes - size_i64;
        if remaining > 0 {
            state.next_sample_bytes = remaining;
            return;
        }

        // Sampled — redraw next gap. Loop guards against the rare case
        // where a single huge allocation pushes the counter below zero
        // by more than one draw's worth (design §1).
        let mut next = remaining;
        while next <= 0 {
            next = next.saturating_add(
                i64::try_from(state.rng.draw_exponential(inner.sample_rate_bytes))
                    .unwrap_or(i64::MAX),
            );
        }
        state.next_sample_bytes = next;

        let timestamp_ns = stamp(inner.timestamp_mode);

        // Stack capture into a stack-resident buffer. The `RefMut`
        // is intentionally held across `capture` and the queue push:
        // if those somehow allocated (they shouldn't — see module
        // docs), the reentrancy guard would correctly trip.
        // SAFETY: `Unwinder::install` was called in `MemoryProfiler::install`,
        // which is the only code path that publishes `inner`. Not called
        // from inside a SIGSEGV handler.
        let mut frames = [0u64; DEFAULT_MAX_FRAMES];
        let result = unsafe { inner.unwinder.capture(&mut frames) };

        let sample = RawAlloc {
            tid: current_tid(),
            size: size as u64,
            addr: ptr as u64,
            ts_ns: timestamp_ns,
            frames,
            frame_count: result.frames_written.min(DEFAULT_MAX_FRAMES) as u8,
        };

        inner.rings.push_alloc(sample);
    });
}

/// Allocator hook for dealloc. No-op when liveset tracking is off.
///
/// SAFETY: must be allocator-quiet — see module docs.
#[inline]
pub(crate) fn on_dealloc(inner: &MemoryProfilerInner, ptr: *mut u8, size: usize) {
    if !inner.track_liveset {
        return;
    }
    let timestamp_ns = stamp(inner.timestamp_mode);
    let sample = RawFree {
        tid: current_tid(),
        addr: ptr as u64,
        size: size as u64,
        ts_ns: timestamp_ns,
    };
    inner.rings.push_free(sample);
}

/// Allocator hook for realloc. Decomposes into free-of-old +
/// alloc-of-new per design §3 ("realloc handling", matches jemalloc
/// convention).
///
/// Only call AFTER the inner realloc returns a non-null `new_ptr` —
/// otherwise the old pointer is still live and must not be recorded
/// as freed.
///
/// SAFETY: must be allocator-quiet — see module docs.
#[inline]
pub(crate) fn on_realloc(
    inner: &MemoryProfilerInner,
    old_ptr: *mut u8,
    old_size: usize,
    new_ptr: *mut u8,
    new_size: usize,
) {
    on_dealloc(inner, old_ptr, old_size);
    on_alloc(inner, new_ptr, new_size);
}
