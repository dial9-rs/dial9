#![deny(clippy::arithmetic_side_effects)]
//! Allocator hook — the hot path called from `Dial9Allocator::alloc`/etc.
//!
//! # Soundness contract
//!
//! Every function in this module must be **allocation-free** — it must not
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

use crate::memory_profiling::profiler::{Liveset, MemoryProfilerInner};
use crate::memory_profiling::ring::{DEFAULT_MAX_FRAMES, RawAlloc, RawFree};
use dial9_core::clock::clock_monotonic_ns;
use dial9_core::sampling::SplitMix64;
use dial9_core::thread::current_tid;

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
            let now = clock_monotonic_ns();
            now.wrapping_mul(0x517cc1b727220a95) ^ nonce ^ (inner as *const _ as u64)
        }
    };
    state.rng = SplitMix64::new(seed);
    state.next_sample_bytes = next_gap(&mut state.rng, inner.sample_rate_bytes);
    state.initialized = true;
}

/// Draw the next bytes-until-sample gap.
///
/// `sample_rate_bytes == 1` is the magic "sample every allocation" mode:
/// we return 0, which makes the next decision (`counter - size`) go ≤ 0
/// for any positive `size`, triggering a sample without consulting the
/// PRNG. This avoids the surprise where mean=1 exponential draws would
/// otherwise produce ~63% per-allocation sampling for `size = 1`
/// allocations due to the exponential's variance around its mean.
///
/// `sample_rate_bytes == 0` is rejected at config build time, so this
/// function only ever sees values `>= 1`.
///
/// For `sample_rate_bytes >= 2`, we draw from an exponential
/// distribution with the given mean.
#[inline]
fn next_gap(rng: &mut SplitMix64, sample_rate_bytes: u64) -> i64 {
    if sample_rate_bytes == 1 {
        return 0;
    }
    i64::try_from(rng.draw_exponential(sample_rate_bytes)).unwrap_or(i64::MAX)
}

/// Allocator hook: called from `Dial9Allocator::alloc` after the inner
/// allocation succeeds (`ptr` is non-null).
///
/// SAFETY: must be allocation-free — see module docs.
#[inline]
pub(crate) fn on_alloc(inner: &MemoryProfilerInner, ptr: *mut u8, size: usize) {
    // `try_with` returns `Err` if the TLS slot is being destroyed during
    // thread teardown — silently skip those allocations rather than
    // risking UB. Logging is forbidden here (allocation-free contract).
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

        let remaining = state.next_sample_bytes.saturating_sub(size_i64);
        if remaining > 0 {
            state.next_sample_bytes = remaining;
            return;
        }

        // Sampled this allocation. Draw the gap to the next sample.
        //
        // We deliberately do NOT loop to "consume" the deficit when a
        // single allocation overshoots the counter by more than one
        // draw. Each `RawAlloc` carries its own `size` field, so
        // downstream rate estimators weight samples by allocation
        // size — burning one PRNG draw vs. many doesn't change
        // per-allocation sampling probability over the long run, and
        // looping was a footgun: at `sample_rate_bytes = 1` and a
        // 1 GiB allocation, a `while next <= 0 { next += draw(1); }`
        // loop ran ~1 billion times inside the allocator hook. A
        // single fresh draw avoids the pathological case while
        // preserving unbiasedness of size-weighted rate estimates.
        //
        // `next_gap` short-circuits the draw entirely when
        // `sample_rate_bytes == 1` (the magic "sample every
        // allocation" value), so that mode never touches the PRNG.
        // `0` is rejected at config build time.
        state.next_sample_bytes = next_gap(&mut state.rng, inner.sample_rate_bytes);

        let timestamp_ns = clock_monotonic_ns();

        // Stack capture into a stack-resident buffer. The `RefMut`
        // is intentionally held across `capture` and the queue push:
        // if those somehow allocated (they shouldn't — see module
        // docs), the reentrancy guard would correctly trip.
        // SAFETY: `Unwinder::install` was called and succeeded in
        // `MemoryProfiler::install` before `inner` was published via
        // `OnceLock::set`. The unwinder's SIGSEGV handler is installed.
        // This is safe because:
        // 1. We're not inside a signal handler (allocator hooks are not
        //    signal-safe, so this is guaranteed by the allocator contract).
        // 2. The stack is valid (normal allocation path, not corrupted).
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

        // Insert into the producer-side liveset OUTSIDE the RefCell borrow.
        // The borrow is still held (we're inside the try_with closure), but
        // scc::HashIndex::insert is allocation-free on the hot path (it uses
        // epoch-based reclamation internally with no heap allocation per insert
        // on the common path).
        //
        // **Stale-entry handling.** `scc::HashIndex::insert` returns `Err` and
        // does NOT overwrite when the key already exists. The address space is
        // reused by the OS allocator, so a stale entry can land here in two
        // cases:
        //   1. The matching dealloc was skipped during thread-teardown by the
        //      OPT_OUT sentinel (see `opt_out.rs`).
        //   2. The matching dealloc's `RawFree` was dropped because the free
        //      queue was full (`MemoryProfileOverflowEvent` was emitted).
        // In both cases we'd otherwise emit a `FreeEvent` later carrying the
        // *old* allocation's `(size, ts_ns)` — wrong data. Detect the failure
        // and fall through to the `entry()` API to atomically overwrite (or
        // insert, if a concurrent dealloc cleared the entry between our
        // `insert` and `entry`). The hot path stays on the cheap lock-free
        // `insert`; only the rare stale case pays the bucket-lock cost.
        //
        // **OPT_OUT init ordering invariant.** `check_shutdown()` MUST be
        // called before any `liveset` op on this thread. It eagerly initialises
        // the `LIFETIME_GUARD` TLS *before* `sdd`'s lazy TLS slot, so the
        // destructor order at thread exit is `sdd` → `LIFETIME_GUARD`, with
        // the guard flipping `IS_SHUTTING_DOWN = true` after sdd is gone. If
        // anyone moves the `liveset.insert` call site, `check_shutdown` must
        // be called first on every code path that touches the liveset, or
        // we'll panic in `sdd::Collector::current()` during teardown.
        if let Some(liveset) = &inner.liveset
            && !crate::memory_profiling::opt_out::check_shutdown()
        {
            let key = ptr as u64;
            let val = (size as u64, timestamp_ns);
            if let Err((k, v)) = liveset.insert(key, val) {
                use scc::hash_index::Entry;
                match liveset.entry(k) {
                    Entry::Occupied(o) => o.update(v),
                    Entry::Vacant(ve) => {
                        // Concurrent dealloc removed the stale entry between our
                        // `insert` and `entry` — treat as fresh insert.
                        let _ = ve.insert_entry(v);
                    }
                }
            }
        }
    });
}

/// Allocator hook for dealloc. With the producer-side liveset, only pushes
/// a `RawFree` if the address was previously sampled (filtering ~99.9% of
/// deallocs on the producer side).
///
/// **OPT_OUT init ordering invariant.** `check_shutdown()` MUST be called
/// before any `liveset` op. See the matching comment on `on_alloc`'s liveset
/// branch and `opt_out.rs` for the full mechanism.
///
/// **Shutdown drain.** During TLS teardown the producer can't safely peek
/// the liveset (sdd's TLS may be destroyed), so it pushes a
/// `RawFree { shutdown: true, .. }` carrying only `addr`. The consolidator
/// (running on a different, healthy thread) does the liveset peek/remove
/// and emits a `FreeEvent` on hit. This recovers dying-thread frees and
/// bounds liveset growth across thread churn. See
/// `MemoryProfileSource::handle_free` for the consumer side, including the
/// `alloc_ts_ns >= free.ts_ns` race-detection check.
///
/// **Re-entrancy guard.** The non-shutdown branch takes the
/// `SAMPLE_STATE` `RefCell` borrow as a per-thread re-entrancy guard,
/// symmetric with `on_alloc`. `liveset.entry` acquires a per-bucket
/// write lock, and `scc`'s epoch-based reclamation (via `sdd`) can free
/// retired buckets when an epoch guard drops — so a re-entrant
/// `on_dealloc` from inside `scc` internals could in theory hit the
/// same bucket the outer `entry` already holds locked and deadlock
/// against itself. The guard makes that impossible by short-circuiting
/// the inner call. The shutdown branch sits *outside* the guard because
/// (a) it never touches `scc`/`sdd` (just a queue-CAS push) and (b)
/// `SAMPLE_STATE`'s `LocalKey` may already be destroyed late in TLS
/// teardown — `try_with` would return `Err` and silently drop the
/// dying thread's free, defeating the shutdown drain.
///
/// SAFETY: must be allocation-free — see module docs.
#[inline]
pub(crate) fn on_dealloc(inner: &MemoryProfilerInner, ptr: *mut u8, _size: usize) {
    let Some(liveset) = &inner.liveset else {
        return;
    };
    let addr = ptr as u64;
    if crate::memory_profiling::opt_out::check_shutdown() {
        // Shutdown drain: producer can't touch scc here. Push a flagged
        // RawFree and let the consolidator do the lookup. `size` and
        // `alloc_ts_ns` are placeholders (0); the consolidator fills them
        // in from its own peek_with. This is allocation-free and only
        // touches the queue. Intentionally outside the SAMPLE_STATE
        // borrow guard — see the function doc.
        inner.rings.push_free(RawFree {
            tid: current_tid(),
            addr,
            ts_ns: clock_monotonic_ns(),
            size: 0,
            alloc_ts_ns: 0,
            shutdown: true,
        });
        return;
    }
    // Non-shutdown branch: take the SAMPLE_STATE borrow as a re-entrancy
    // guard. If a prior `on_alloc`/`on_dealloc` frame on this thread is
    // still on the stack and holds the borrow, skip — otherwise the
    // re-entrant `liveset.entry` could deadlock against the outer one on
    // a shared bucket lock, and any allocation triggered inside `scc`
    // would otherwise be unguarded.
    let _ = SAMPLE_STATE.try_with(|cell| {
        let Ok(_state) = cell.try_borrow_mut() else {
            return;
        };
        if let Some((size, alloc_ts_ns)) = take_liveset_entry(liveset, addr) {
            let sample = RawFree {
                tid: current_tid(),
                addr,
                ts_ns: clock_monotonic_ns(),
                size,
                alloc_ts_ns,
                shutdown: false,
            };
            inner.rings.push_free(sample);
        }
    });
}

/// Atomically read and remove the liveset entry for `addr`, returning its
/// `(size, alloc_ts_ns)`.
///
/// Uses the `entry()` API rather than a separate peek and remove: the bucket
/// lock is held for the duration of the `Entry`, so a concurrent `on_alloc`
/// overwriting the same address cannot interleave between the read and the
/// remove, and the returned metadata always describes the entry that was
/// actually removed.
///
/// **Caller invariant: exclusive ownership of `addr`.** Atomicity alone does
/// not make the popped entry belong to the allocation being freed — if the
/// address has already been released to the inner allocator, another thread
/// can have been handed it and overwritten the entry, and we would pop the
/// *new* allocation's metadata. Every caller must still exclusively own the
/// address:
/// - `on_dealloc` runs before `Dial9Allocator::dealloc` forwards to the
///   inner allocator, so `ptr` has not been released yet.
/// - `on_realloc_before` runs before `Dial9Allocator::realloc` forwards, for
///   the same reason. See [`ReallocState`].
///
/// Must be called with the `SAMPLE_STATE` re-entrancy guard held and after
/// `check_shutdown()` — see the callers.
///
/// SAFETY: must be allocation-free — see module docs.
#[inline]
fn take_liveset_entry(liveset: &Liveset, addr: u64) -> Option<(u64, u64)> {
    use scc::hash_index::Entry;
    if let Entry::Occupied(o) = liveset.entry(addr) {
        let val = *o.get();
        o.remove_entry();
        Some(val)
    } else {
        None
    }
}

/// State carried across the inner allocator's `realloc` call.
///
/// **Why `realloc` cannot just call `on_dealloc` afterwards.** `on_dealloc`
/// is sound because `Dial9Allocator::dealloc` runs it *before* forwarding to
/// `self.0.dealloc(ptr)`: the calling thread still exclusively owns `ptr`, so
/// no concurrent `on_alloc` can be inserting that address. `realloc` has no
/// such ordering available — the inner `realloc` may release the old block
/// internally, so by the time it returns, another thread can already have
/// been handed that address, sampled it, and overwritten the liveset entry.
/// Resolving the old address afterwards would then pop the *new* allocation's
/// metadata: the emitted `RawFree` carries the wrong `(size, alloc_ts_ns)`,
/// and a live allocation loses its liveset entry so its real free is never
/// reported.
///
/// So the liveset removal moves *before* the inner realloc, while we still
/// own `ptr`, and only the emit happens after — once we know whether the
/// realloc actually succeeded. That restores the same exclusive-ownership
/// invariant the `dealloc` path relies on. See [`take_liveset_entry`].
///
/// Stack-resident and `Copy`-free by construction; carrying it across the
/// inner call allocates nothing.
pub(crate) struct ReallocState {
    /// Sampled *before* the inner realloc, i.e. before the old block can be
    /// released. Used as the emitted `RawFree`'s timestamp, which keeps the
    /// consolidator's `alloc_ts_ns >= ts_ns` address-reuse guard sound for
    /// shutdown-flagged frees (see `MemoryProfileSource::handle_free`): any
    /// later allocation of this address is necessarily stamped after it.
    ts_ns: u64,
    /// `(size, alloc_ts_ns)` popped from the liveset, if the old block was
    /// sampled. `None` means it was not.
    taken: Option<(u64, u64)>,
    /// This thread is in TLS teardown and cannot touch `scc`, so nothing was
    /// popped; the consolidator resolves the address instead. Mirrors
    /// `on_dealloc`'s shutdown drain.
    shutdown: bool,
}

/// Allocator hook run **before** the inner `realloc`. Pops the old block's
/// liveset entry while the caller still exclusively owns `old_ptr`.
///
/// Pairs with exactly one of [`on_realloc_success`] or [`on_realloc_failed`].
///
/// **OPT_OUT init ordering invariant.** `check_shutdown()` MUST be called
/// before any `liveset` op. See `on_alloc`'s liveset branch and `opt_out.rs`
/// for the full mechanism.
///
/// **Re-entrancy guard.** Takes the `SAMPLE_STATE` borrow around the liveset
/// op, symmetric with `on_dealloc`; see its doc for why. The shutdown branch
/// sits outside the guard for the same reason it does there.
///
/// SAFETY: must be allocation-free — see module docs.
#[inline]
pub(crate) fn on_realloc_before(inner: &MemoryProfilerInner, old_ptr: *mut u8) -> ReallocState {
    let ts_ns = clock_monotonic_ns();
    let Some(liveset) = &inner.liveset else {
        return ReallocState {
            ts_ns,
            taken: None,
            shutdown: false,
        };
    };
    if crate::memory_profiling::opt_out::check_shutdown() {
        return ReallocState {
            ts_ns,
            taken: None,
            shutdown: true,
        };
    }
    let addr = old_ptr as u64;
    let mut taken = None;
    let _ = SAMPLE_STATE.try_with(|cell| {
        let Ok(_state) = cell.try_borrow_mut() else {
            return;
        };
        taken = take_liveset_entry(liveset, addr);
    });
    ReallocState {
        ts_ns,
        taken,
        shutdown: false,
    }
}

/// Allocator hook run after a **successful** inner `realloc`. Emits the
/// free-of-old captured by [`on_realloc_before`], then the alloc-of-new, per
/// design §3 ("realloc handling", matches jemalloc convention).
///
/// The free's timestamp was sampled before the inner realloc and the alloc's
/// is sampled here, so the free strictly precedes the alloc. The
/// consolidator's merge-by-timestamp drain therefore orders them correctly
/// even for an in-place realloc that returns the same address.
///
/// SAFETY: must be allocation-free — see module docs.
#[inline]
pub(crate) fn on_realloc_success(
    inner: &MemoryProfilerInner,
    old_ptr: *mut u8,
    state: ReallocState,
    new_ptr: *mut u8,
    new_size: usize,
) {
    if state.shutdown {
        // Shutdown drain: the producer could not touch scc, so push a
        // flagged RawFree and let the consolidator do the lookup. `size` and
        // `alloc_ts_ns` are placeholders (0). Mirrors `on_dealloc`.
        inner.rings.push_free(RawFree {
            tid: current_tid(),
            addr: old_ptr as u64,
            ts_ns: state.ts_ns,
            size: 0,
            alloc_ts_ns: 0,
            shutdown: true,
        });
    } else if let Some((size, alloc_ts_ns)) = state.taken {
        inner.rings.push_free(RawFree {
            tid: current_tid(),
            addr: old_ptr as u64,
            ts_ns: state.ts_ns,
            size,
            alloc_ts_ns,
            shutdown: false,
        });
    }
    on_alloc(inner, new_ptr, new_size);
}

/// Allocator hook run after a **failed** inner `realloc`. The old block is
/// still live, so the entry [`on_realloc_before`] popped has to go back and
/// no free may be emitted.
///
/// No concurrent `on_alloc` can be racing us for this address: a failed
/// realloc never releases `old_ptr`, so the calling thread still owns it and
/// the plain `insert` cannot collide.
///
/// SAFETY: must be allocation-free — see module docs.
#[inline]
pub(crate) fn on_realloc_failed(
    inner: &MemoryProfilerInner,
    old_ptr: *mut u8,
    state: ReallocState,
) {
    let Some(val) = state.taken else {
        return;
    };
    // `taken` is only ever `Some` when the liveset is on and we were not
    // shutting down, but re-check the shutdown flag: `on_realloc_before` and
    // this call straddle the inner realloc, and the thread could have entered
    // TLS teardown in between, after which touching `scc` would panic in
    // `sdd::Collector::current()`. Dropping the entry is the safe outcome —
    // `on_alloc` overwrites stale entries when the address is reused.
    let Some(liveset) = &inner.liveset else {
        return;
    };
    if crate::memory_profiling::opt_out::check_shutdown() {
        return;
    }
    let addr = old_ptr as u64;
    let _ = SAMPLE_STATE.try_with(|cell| {
        let Ok(_state) = cell.try_borrow_mut() else {
            return;
        };
        // Cannot collide: we still own `addr` (see the doc above), so the
        // only way this errs is a stale entry that `on_alloc` would have
        // overwritten anyway.
        let _ = liveset.insert(addr, val);
    });
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;
    use crate::memory_profiling::profiler::MemoryProfilerInner;
    use crate::memory_profiling::ring::RingBuffers;
    use crate::unwinder::Unwinder;
    use dial9_core::handle::Dial9Handle;
    use dial9_core::sampling::SplitMix64;
    use std::sync::Arc;

    /// Reset this thread's sampling state to a deterministic seed.
    ///
    /// Test-only. Initialises TLS so subsequent `on_alloc` calls draw
    /// from a known PRNG sequence regardless of `current_tid()` or
    /// previous allocations on this thread.
    fn seed_thread_sampling_state(seed: u64, sample_rate_bytes: u64) {
        SAMPLE_STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            state.rng = SplitMix64::new(seed);
            state.next_sample_bytes = next_gap(&mut state.rng, sample_rate_bytes);
            state.initialized = true;
        });
    }

    /// Replay the production sampling logic in pure Rust to predict how
    /// many samples a given sequence of allocation sizes will produce
    /// against a freshly seeded counter.
    ///
    /// Mirrors `on_alloc`'s decision step exactly: subtract the size,
    /// sample if the counter goes ≤ 0, redraw a fresh gap via `next_gap`.
    fn predict_sample_count(
        seed: u64,
        sample_rate_bytes: u64,
        sizes: impl IntoIterator<Item = u64>,
    ) -> u64 {
        let mut rng = SplitMix64::new(seed);
        let mut counter = next_gap(&mut rng, sample_rate_bytes);
        let mut samples = 0u64;
        for size in sizes {
            let size_i64 = i64::try_from(size).unwrap_or(i64::MAX);
            counter = counter.saturating_sub(size_i64);
            if counter > 0 {
                continue;
            }
            samples = samples.saturating_add(1);
            counter = next_gap(&mut rng, sample_rate_bytes);
        }
        samples
    }

    /// Build a `MemoryProfilerInner` for direct hook testing, bypassing
    /// the install path and the global `ACTIVE` slot.
    fn make_inner(sample_rate_bytes: u64, ring_capacity: usize) -> MemoryProfilerInner {
        let unwinder = Unwinder::install().expect("unwinder install");
        let rings = Arc::new(RingBuffers::new(ring_capacity, ring_capacity));
        MemoryProfilerInner {
            unwinder,
            handle: Dial9Handle::disabled(),
            rings,
            sample_rate_bytes,
            liveset: None,
            rng_seed: Some(0),
        }
    }

    /// Drive `n` allocations of `size` bytes through `on_alloc` with a
    /// freshly seeded counter, returning the number of samples that
    /// landed in the alloc ring. Runs on a dedicated thread so the TLS
    /// state is fresh per scenario.
    fn run_scenario(seed: u64, sample_rate_bytes: u64, size: usize, n: usize) -> u64 {
        let inner = Arc::new(make_inner(
            sample_rate_bytes,
            // Big enough that we never drop samples in the test.
            n.max(16),
        ));
        let inner_for_thread = Arc::clone(&inner);
        std::thread::spawn(move || {
            seed_thread_sampling_state(seed, sample_rate_bytes);
            // A bogus pointer is fine — `on_alloc` only stores it as `addr`.
            let bogus_ptr = 0xDEAD_BEEF_usize as *mut u8;
            for _ in 0..n {
                on_alloc(&inner_for_thread, bogus_ptr, size);
            }
        })
        .join()
        .expect("scenario thread panicked");

        let mut count = 0u64;
        while inner.rings.alloc_queue.pop().is_some() {
            count = count.saturating_add(1);
        }
        let dropped = inner
            .rings
            .dropped_allocs
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(dropped, 0, "ring overflowed: {dropped} samples dropped");
        count
    }

    /// Drive a sequence of arbitrary allocation sizes through `on_alloc`
    /// with a freshly seeded counter, returning the sizes (in bytes) of
    /// every sampled allocation. Used by the cross-strategy estimator
    /// tests.
    fn run_scenario_with_sizes(
        seed: u64,
        sample_rate_bytes: u64,
        sizes: Vec<u64>,
        ring_capacity: usize,
    ) -> Vec<u64> {
        let inner = Arc::new(make_inner(sample_rate_bytes, ring_capacity));
        let inner_for_thread = Arc::clone(&inner);
        std::thread::spawn(move || {
            seed_thread_sampling_state(seed, sample_rate_bytes);
            let bogus_ptr = 0xDEAD_BEEF_usize as *mut u8;
            for size in sizes {
                // Cap at usize::MAX defensively; in practice sizes fit.
                on_alloc(&inner_for_thread, bogus_ptr, size as usize);
            }
        })
        .join()
        .expect("scenario thread panicked");

        let dropped = inner
            .rings
            .dropped_allocs
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(dropped, 0, "ring overflowed: {dropped} samples dropped");

        let mut sample_sizes = Vec::new();
        while let Some(raw) = inner.rings.alloc_queue.pop() {
            sample_sizes.push(raw.size);
        }
        sample_sizes
    }

    /// Unbiased Horvitz–Thompson estimator of total allocated bytes
    /// from a Poisson-sampled trace.
    ///
    /// Each sampled allocation of size `s` is weighted by
    /// `1 / P(sample) = 1 / (1 - exp(-s / mean))`, so the estimate is
    /// `Σ_sampled s_i / (1 - exp(-s_i / mean))`. For `s << mean` this
    /// approaches `count * mean`; for `s >> mean` it approaches the raw
    /// sum of sample sizes (since every alloc is sampled with
    /// probability ~1).
    fn estimate_total_bytes(samples: &[u64], sample_rate_bytes: u64) -> f64 {
        let mean = sample_rate_bytes as f64;
        samples
            .iter()
            .map(|&s| {
                let p = 1.0 - (-(s as f64) / mean).exp();
                (s as f64) / p
            })
            .sum()
    }

    #[test]
    fn sample_count_matches_prediction_for_small_allocs() {
        // Small: alloc size << sample rate. Most allocs miss; samples
        // are sparse and statistical correctness depends on the gap
        // distribution.
        let seed = 0xABCD_EF01;
        let sample_rate = 4096;
        let size = 64;
        let n = 10_000;

        let predicted =
            predict_sample_count(seed, sample_rate, std::iter::repeat_n(size as u64, n));
        let actual = run_scenario(seed, sample_rate, size, n);

        assert_eq!(
            actual, predicted,
            "small allocs: actual {actual} != predicted {predicted}"
        );

        // Sanity: roughly n*size/rate samples expected (here ~156).
        let approx = (n as u64 * size as u64) / sample_rate;
        assert!(
            actual.abs_diff(approx) < approx,
            "small allocs sanity: actual {actual} far from approx {approx}"
        );
    }

    #[test]
    fn sample_count_matches_prediction_for_large_allocs() {
        // Large: alloc size ~= sample rate. Each alloc has ~63% chance
        // of being sampled; roughly n samples expected for n allocs.
        let seed = 0x1234_5678;
        let sample_rate = 1024;
        let size = 1024;
        let n = 1_000;

        let predicted =
            predict_sample_count(seed, sample_rate, std::iter::repeat_n(size as u64, n));
        let actual = run_scenario(seed, sample_rate, size, n);

        assert_eq!(
            actual, predicted,
            "large allocs: actual {actual} != predicted {predicted}"
        );
        assert!(actual > 0, "large allocs should produce some samples");
    }

    #[test]
    fn sample_count_matches_prediction_for_very_large_allocs() {
        // Very large: alloc size >> sample rate. Every allocation
        // samples (counter is always blown past zero).
        let seed = 0xCAFE_BABE;
        let sample_rate = 1024;
        let size = 1024 * 1024; // 1 MiB
        let n = 100;

        let predicted =
            predict_sample_count(seed, sample_rate, std::iter::repeat_n(size as u64, n));
        let actual = run_scenario(seed, sample_rate, size, n);

        assert_eq!(
            actual, predicted,
            "very large allocs: actual {actual} != predicted {predicted}"
        );
        // Every allocation should sample.
        assert_eq!(actual, n as u64, "every very-large alloc should sample");
    }

    #[test]
    fn sample_rate_one_samples_every_allocation() {
        // sample_rate = 1 means "sample every allocation". This is the
        // case that previously caused pathological loop iterations
        // inside `on_alloc` for large allocations; with the single
        // fresh draw the loop is gone.
        let seed = 0x4242_4242;
        let sample_rate = 1;
        let n = 500;

        // Mix of sizes including a 1 GiB-equivalent (capped to fit in
        // the ring capacity test budget — 1 MiB is enough to confirm
        // we don't burn cycles in a redraw loop).
        for size in [1, 64, 4096, 1024 * 1024] {
            let actual = run_scenario(seed, sample_rate, size, n);
            assert_eq!(
                actual, n as u64,
                "sample_rate=1 with size={size}: every alloc should sample, got {actual}"
            );
        }
    }

    #[test]
    fn sample_rate_one_with_huge_alloc_is_fast() {
        // Regression test for the sample-rate-1 footgun: previously,
        // `on_alloc(size = 1 GiB)` with `sample_rate_bytes = 1` ran a
        // loop that called `draw_exponential` ~1 billion times to
        // climb the counter back from -1 GiB. With the single-draw
        // redraw + the `== 1` magic-value short-circuit, even huge
        // allocations complete in microseconds.
        let seed = 0xFEED_FACE;
        let sample_rate = 1;
        let huge = 1024 * 1024 * 1024; // 1 GiB
        let n = 16;

        let start = std::time::Instant::now();
        let actual = run_scenario(seed, sample_rate, huge, n);
        let elapsed = start.elapsed();

        assert_eq!(actual, n as u64);
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "sample_rate=1 with 1 GiB allocs took {elapsed:?}; the redraw loop \
             must have come back"
        );
    }

    /// Demonstrates that the Horvitz–Thompson estimator recovers the
    /// total bytes allocated regardless of allocation size distribution,
    /// when sampled at a fixed rate.
    ///
    /// Strategies (each totalling 1 MiB):
    ///   1. Tiny: 1B × 1,048,576 — far below sample rate.
    ///   2. Small: 64B × 16,384.
    ///   3. Medium: 1024B × 1024 — same scale as sample rate.
    ///   4. Large: 64 KiB × 16 — well above sample rate.
    ///   5. Huge: 1 MiB × 1 — single allocation.
    ///   6. Random mix.
    ///
    /// Each strategy is run with `sample_rate_bytes = 512`. The estimated
    /// total bytes from `Σ s_i / (1 - exp(-s_i/mean))` should equal 1 MiB
    /// within ~10% (statistical noise). Naive `Σ s_i` would NOT — it
    /// undercounts strategies dominated by small allocations.
    #[test]
    fn unbiased_estimator_recovers_total_bytes_across_strategies() {
        let total_bytes = 1024u64 * 1024; // 1 MiB
        let sample_rate: u64 = 512;
        let seed = 0x5EED_5EED;

        let strategies: Vec<(&str, Vec<u64>)> = vec![
            ("tiny 1B", vec![1; total_bytes as usize]),
            ("small 64B", vec![64; (total_bytes / 64) as usize]),
            ("medium 1024B", vec![1024; (total_bytes / 1024) as usize]),
            ("large 64 KiB", vec![65536; (total_bytes / 65536) as usize]),
            ("huge 1 MiB", vec![total_bytes]),
            ("random mix", random_mix_summing_to(seed, total_bytes)),
        ];

        // Ring needs to be big enough for the most-sampled strategy
        // (tiny 1B): expected ~total_bytes / sample_rate ≈ 2048
        // samples. Round up generously.
        let ring_capacity = 1 << 14; // 16 384

        for (label, sizes) in strategies {
            // Sanity: every strategy sums to the same total.
            let actual_total: u64 = sizes.iter().sum();
            assert_eq!(actual_total, total_bytes, "{label} sum mismatch");

            let samples = run_scenario_with_sizes(seed, sample_rate, sizes, ring_capacity);
            assert!(!samples.is_empty(), "{label} produced no samples");

            let estimate = estimate_total_bytes(&samples, sample_rate);
            let truth = total_bytes as f64;
            let relative_error = (estimate - truth).abs() / truth;

            // 15% tolerance covers seed variance for the high-variance
            // "huge 1 MiB" case (n=1, so the single sample IS the
            // estimate). Other strategies converge tighter.
            assert!(
                relative_error < 0.15,
                "{label}: estimated {estimate:.0} bytes, expected {truth:.0} \
                 (rel err {:.2}%, samples: {})",
                relative_error * 100.0,
                samples.len()
            );

            eprintln!(
                "{label}: {} samples, raw sum = {} B, estimate = {:.0} B \
                 (rel err {:.2}%)",
                samples.len(),
                samples.iter().sum::<u64>(),
                estimate,
                relative_error * 100.0
            );
        }
    }

    /// Build a random sequence of allocation sizes summing to exactly
    /// `total_bytes`. Uses `SplitMix64` seeded with `seed` so the test
    /// is deterministic. Sizes are drawn from a log-uniform distribution
    /// over [1, 8192] to exercise a wide dynamic range.
    fn random_mix_summing_to(seed: u64, total_bytes: u64) -> Vec<u64> {
        let mut rng = SplitMix64::new(seed);
        let mut sizes = Vec::new();
        let mut remaining = total_bytes;
        while remaining > 0 {
            // Log-uniform draw in [1, 8192]: 13 bit ranges.
            let bits = rng.next_u64().rem_euclid(13).saturating_add(1);
            let max = 1u64 << bits;
            let size = rng.next_u64().rem_euclid(max).saturating_add(1);
            let size = size.min(remaining);
            sizes.push(size);
            remaining = remaining.saturating_sub(size);
        }
        sizes
    }

    /// Build a `MemoryProfilerInner` with the producer-side liveset enabled,
    /// for tests that exercise the address-reuse / stale-entry path.
    fn make_inner_with_liveset(
        sample_rate_bytes: u64,
        ring_capacity: usize,
    ) -> MemoryProfilerInner {
        let unwinder = Unwinder::install().expect("unwinder install");
        let rings = Arc::new(RingBuffers::new(ring_capacity, ring_capacity));
        let liveset = Arc::new(scc::HashIndex::with_capacity_and_hasher(
            0,
            dial9_trace_format::encoder::FxBuildHasher::default(),
        ));
        MemoryProfilerInner {
            unwinder,
            handle: Dial9Handle::disabled(),
            rings,
            sample_rate_bytes,
            liveset: Some(liveset),
            rng_seed: Some(0),
        }
    }

    /// Reproduces the address-reuse hazard fixed by the stale-entry overwrite
    /// in `on_alloc`. Without overwrite-on-Err, the second `on_alloc` for the
    /// same address silently drops its `(size, ts)` and a subsequent
    /// `on_dealloc` reports the *first* allocation's metadata.
    #[test]
    fn on_alloc_overwrites_stale_liveset_entry() {
        let inner = Arc::new(make_inner_with_liveset(1, 64));
        let inner_for_thread = Arc::clone(&inner);
        std::thread::spawn(move || {
            seed_thread_sampling_state(0xBEEF_BEEF, 1);
            let addr = 0xCAFE_F00D_usize as *mut u8;

            // First sampled allocation: size 100. on_alloc inserts
            // (100, ts1) into the liveset.
            on_alloc(&inner_for_thread, addr, 100);

            // Simulate the OPT_OUT skip / queue-overflow path: the matching
            // dealloc never reaches `on_dealloc`. The (100, ts1) entry is
            // now stale.

            // Second sampled allocation at the same address: size 200. With
            // the fix, on_alloc detects the duplicate-key Err from
            // `scc::HashIndex::insert` and overwrites the entry with
            // (200, ts2).
            on_alloc(&inner_for_thread, addr, 200);

            // Now dealloc: must observe size = 200, NOT the stale 100.
            on_dealloc(&inner_for_thread, addr, 200);
        })
        .join()
        .expect("scenario thread");

        // Drain the queues. We expect 2 allocs and 1 free.
        let mut allocs = Vec::new();
        while let Some(a) = inner.rings.alloc_queue.pop() {
            allocs.push(a);
        }
        let mut frees = Vec::new();
        while let Some(f) = inner.rings.free_queue.pop() {
            frees.push(f);
        }
        assert_eq!(allocs.len(), 2, "both allocs should have been recorded");
        assert_eq!(allocs[0].size, 100);
        assert_eq!(allocs[1].size, 200);

        assert_eq!(frees.len(), 1, "the dealloc should have produced a free");
        assert_eq!(
            frees[0].size, 200,
            "free must report the second alloc's size, not the stale first \
             alloc's; without the overwrite fix this would be 100"
        );
        assert_eq!(
            frees[0].alloc_ts_ns, allocs[1].ts_ns,
            "free's alloc_ts_ns must match the second alloc, not the stale \
             first one; without the overwrite fix this would be allocs[0].ts_ns"
        );
    }

    /// Re-entrancy guard for `on_dealloc`. If a prior frame on this thread is
    /// already inside `on_alloc`/`on_dealloc` and holds the `SAMPLE_STATE`
    /// borrow, a re-entrant `on_dealloc` must skip — otherwise the inner
    /// call's `liveset.remove` could try to take a bucket write lock that
    /// the outer call already holds (same-thread deadlock if `scc` happens
    /// to recycle a stale liveset key into the very bucket the outer
    /// operation is mutating).
    ///
    /// This test holds the `SAMPLE_STATE::RefCell` borrow explicitly to
    /// simulate the "outer call still on the stack" condition, then calls
    /// `on_dealloc`. Without the guard, `on_dealloc` would peek/remove the
    /// liveset entry and push a `RawFree`. With the guard, both side
    /// effects must be suppressed.
    #[test]
    fn on_dealloc_skips_when_sample_state_is_already_borrowed() {
        let inner = Arc::new(make_inner_with_liveset(1, 64));
        let inner_for_thread = Arc::clone(&inner);
        let addr_u64 = 0xFEED_FACE_u64;

        std::thread::spawn(move || {
            seed_thread_sampling_state(0xDEAD_C0DE, 1);
            let addr = addr_u64 as usize as *mut u8;

            // Seed the liveset with a real entry via on_alloc so we have
            // something for a re-entrant on_dealloc to find.
            on_alloc(&inner_for_thread, addr, 64);

            // Simulate being mid-flight in on_alloc / on_dealloc: hold the
            // SAMPLE_STATE borrow before calling on_dealloc. With the
            // re-entrancy guard, on_dealloc must observe the borrow and
            // bail without touching the liveset or queue.
            SAMPLE_STATE.with(|cell| {
                let _outer_borrow = cell
                    .try_borrow_mut()
                    .expect("test thread holds the only borrow");
                on_dealloc(&inner_for_thread, addr, 64);
            });
        })
        .join()
        .expect("scenario thread");

        // The liveset entry must still be present — the inner on_dealloc
        // must not have removed it.
        let liveset = inner.liveset.as_ref().expect("liveset configured");
        assert!(
            liveset.peek_with(&addr_u64, |_, _| ()).is_some(),
            "re-entrant on_dealloc must skip the liveset.remove; entry was \
             unexpectedly removed (the guard is not in place)"
        );

        // And no RawFree should have been pushed.
        assert!(
            inner.rings.free_queue.is_empty(),
            "re-entrant on_dealloc must skip the RawFree push; the queue is \
             non-empty (the guard is not in place)"
        );
    }

    /// The core of the realloc ordering fix.
    ///
    /// `Dial9Allocator::realloc` forwards to the inner allocator *before* it
    /// can run any hook, and the inner realloc may release the old block on
    /// the way. Another thread can therefore be handed that address, sample
    /// it, and overwrite the liveset entry before our hook looks at it.
    ///
    /// This test models exactly that interleaving deterministically: the
    /// recycling `on_alloc` runs between `on_realloc_before` and
    /// `on_realloc_success`. Because the entry now comes out *before* the
    /// inner realloc, the emitted free must still describe the ORIGINAL
    /// allocation, and the recycled allocation must keep its liveset entry.
    ///
    /// Against the old `on_realloc` (dealloc-of-old resolved after the inner
    /// realloc) both assertions fail: the free reports the recycled
    /// allocation's size, and the recycled allocation's entry is gone, so its
    /// real free is never reported.
    #[test]
    fn realloc_pops_old_entry_before_the_inner_realloc_runs() {
        let inner = Arc::new(make_inner_with_liveset(1, 64));
        let inner_for_thread = Arc::clone(&inner);
        let old_addr_u = 0xCAFE_0000_usize;
        let new_addr_u = 0xCAFE_1000_usize;

        std::thread::spawn(move || {
            seed_thread_sampling_state(0xA11C_0DE5, 1);
            let (old_addr, new_addr) = (old_addr_u as *mut u8, new_addr_u as *mut u8);

            // The allocation being realloc'd: size 100.
            on_alloc(&inner_for_thread, old_addr, 100);

            // Hook runs while we still exclusively own `old_addr`.
            let state = on_realloc_before(&inner_for_thread, old_addr);

            // Inner realloc releases the old block, and a concurrent thread
            // is handed the same address and samples it. With the entry
            // already popped this is a clean insert, not an overwrite.
            on_alloc(&inner_for_thread, old_addr, 999);

            on_realloc_success(&inner_for_thread, old_addr, state, new_addr, 200);
        })
        .join()
        .expect("scenario thread");

        let mut allocs = Vec::new();
        while let Some(a) = inner.rings.alloc_queue.pop() {
            allocs.push(a);
        }
        let mut frees = Vec::new();
        while let Some(f) = inner.rings.free_queue.pop() {
            frees.push(f);
        }

        assert_eq!(allocs.len(), 3, "original, recycled, and realloc'd-to");
        assert_eq!(allocs[0].size, 100, "original allocation");
        assert_eq!(allocs[1].size, 999, "recycled allocation");
        assert_eq!(allocs[2].size, 200, "realloc'd-to allocation");

        assert_eq!(frees.len(), 1, "exactly one free-of-old");
        assert_eq!(
            frees[0].size, 100,
            "the free must describe the allocation actually being realloc'd \
             away, not the one that recycled its address; resolving the \
             address after the inner realloc would report 999"
        );
        assert_eq!(
            frees[0].alloc_ts_ns, allocs[0].ts_ns,
            "free's alloc_ts_ns must match the original allocation"
        );
        assert!(
            frees[0].ts_ns < allocs[2].ts_ns,
            "free-of-old must be stamped before alloc-of-new so the \
             consolidator's merge-by-timestamp drain orders them correctly \
             (design §3)"
        );

        // The recycled allocation must still be tracked, so its own eventual
        // free is reported.
        let liveset = inner.liveset.as_ref().expect("liveset configured");
        assert_eq!(
            liveset.peek_with(&(old_addr_u as u64), |_, v| v.0),
            Some(999),
            "the recycled allocation must keep its liveset entry; resolving \
             the address after the inner realloc would have removed it"
        );
    }

    /// A failed realloc never releases the old pointer, so the block is still
    /// live: the entry `on_realloc_before` popped has to go back and no free
    /// may be emitted. Without the restore, a still-live sampled allocation
    /// would silently stop being tracked and its real free would be dropped.
    #[test]
    fn failed_realloc_restores_the_liveset_entry() {
        let inner = Arc::new(make_inner_with_liveset(1, 64));
        let inner_for_thread = Arc::clone(&inner);
        let addr_u = 0xF00D_0000_usize;

        std::thread::spawn(move || {
            seed_thread_sampling_state(0xFA11_BACC, 1);
            let addr = addr_u as *mut u8;
            on_alloc(&inner_for_thread, addr, 512);

            let state = on_realloc_before(&inner_for_thread, addr);
            // Inner realloc returned null — the old block is untouched.
            on_realloc_failed(&inner_for_thread, addr, state);

            // The block is still live and tracked, so its real free must
            // still report the original metadata.
            on_dealloc(&inner_for_thread, addr, 512);
        })
        .join()
        .expect("scenario thread");

        let allocs: Vec<_> = std::iter::from_fn(|| inner.rings.alloc_queue.pop()).collect();
        let frees: Vec<_> = std::iter::from_fn(|| inner.rings.free_queue.pop()).collect();

        assert_eq!(allocs.len(), 1, "a failed realloc allocates nothing");
        assert_eq!(
            frees.len(),
            1,
            "only the real dealloc emits a free; the failed realloc must not"
        );
        assert_eq!(
            frees[0].size, 512,
            "the restored entry must carry the original metadata"
        );
        assert_eq!(frees[0].alloc_ts_ns, allocs[0].ts_ns);

        let liveset = inner.liveset.as_ref().expect("liveset configured");
        assert!(
            liveset.peek_with(&(addr_u as u64), |_, _| ()).is_none(),
            "the real dealloc must have drained the restored entry"
        );
    }

    /// An unsampled block realloc'd away must not emit a free, and must not
    /// disturb the liveset. Pins the `taken: None` path through all three
    /// hooks.
    #[test]
    fn realloc_of_never_sampled_block_emits_no_free() {
        // Sample rate high enough that the 8-byte allocations below never
        // sample, so nothing reaches the liveset.
        let inner = Arc::new(make_inner_with_liveset(1 << 30, 64));
        let inner_for_thread = Arc::clone(&inner);
        let old_addr_u = 0x0BAD_0000_usize;
        let new_addr_u = 0x0BAD_1000_usize;

        std::thread::spawn(move || {
            seed_thread_sampling_state(0xDEAD_C0DE, 1 << 30);
            let (old_addr, new_addr) = (old_addr_u as *mut u8, new_addr_u as *mut u8);
            let state = on_realloc_before(&inner_for_thread, old_addr);
            on_realloc_success(&inner_for_thread, old_addr, state, new_addr, 8);
        })
        .join()
        .expect("scenario thread");

        assert!(
            inner.rings.free_queue.is_empty(),
            "a realloc of a never-sampled block must not push a RawFree"
        );
        let liveset = inner.liveset.as_ref().expect("liveset configured");
        assert!(
            liveset.peek_with(&(old_addr_u as u64), |_, _| ()).is_none(),
            "a realloc miss must not leave an entry in the liveset"
        );
    }
}
