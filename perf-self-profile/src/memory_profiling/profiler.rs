//! Install the process-global memory profiler and drain captured allocations.

use crate::memory_profiling::ring::{DEFAULT_MAX_FRAMES, RawAlloc, RawFree, RingBuffers};
use crate::unwinder::Unwinder;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

/// Producer-side liveset: maps allocation address → `(size, timestamp_ns)`.
///
/// See [`MemoryProfilerInner::liveset`] for the rationale on `Arc<...>` and
/// `FxBuildHasher`.
pub(crate) type Liveset =
    scc::HashIndex<u64, (u64, u64), dial9_trace_format::encoder::FxBuildHasher>;

/// Process-global state for the active memory profiler.
///
/// Published via `OnceLock` exactly once per process. Never reclaimed
/// because any thread's allocator hook may be reading this.
pub(crate) struct MemoryProfilerInner {
    pub(crate) unwinder: Unwinder,
    pub(crate) rings: Arc<RingBuffers>,
    pub(crate) sample_rate_bytes: u64,
    /// Producer-side liveset: maps allocation address → (size, timestamp_ns).
    /// `None` when liveset tracking is disabled, avoiding any overhead.
    /// Whether liveset tracking is on is determined by `liveset.is_some()`
    /// — there is no separate boolean flag.
    ///
    /// **Why `Arc<HashIndex>`?** `scc::HashIndex` *is* `Clone`, but the impl
    /// is a *deep clone*: it iterates every entry and reinserts into a brand
    /// new index with independent state (see `scc-2.x` `impl Clone for
    /// HashIndex`). To share one map across all producer threads we need
    /// reference-counted shared ownership, hence the `Arc`. This is unlike
    /// e.g. `dashmap::DashMap` whose `Clone` is a shallow `Arc::clone`.
    ///
    /// **Why `FxBuildHasher`?** `scc` calls the `BuildHasher` per operation
    /// (no internal hash caching — see `scc::hash_table::HashTable::hash`).
    /// For a `u64` pointer key the default `RandomState`/SipHash-1-3 is
    /// ~10–25 ns; `FxHash`'s multiply-shift is ~1–2 ns, and that cost
    /// applies to **every** dealloc when liveset is on (`peek_with` always
    /// hashes regardless of hit/miss). Pointer alignment-bit skew is not a
    /// concern at scc's bucket granularity for the hit rates we see here.
    ///
    /// **Known limitation:** the map is currently unbounded. A service that
    /// leaks sampled allocations indefinitely will grow this map without
    /// limit. The size is bounded in practice by the live sampled allocation
    /// count: at the default 512 KiB sample rate and a 10 GiB live heap,
    /// expect ~20K entries (≈1.3 MiB including scc overhead). Adding a
    /// `max_liveset_entries` cap is tracked as a follow-up; see
    /// `docs/design/memory-profiling.md` §7.
    pub(crate) liveset: Option<Arc<Liveset>>,
    pub(crate) rng_seed: Option<u64>,
}

/// Process-global handle to the installed memory profiler.
pub(crate) static ACTIVE: OnceLock<MemoryProfilerInner> = OnceLock::new();

/// Returns `true` if the memory profiler has been installed in this process.
///
/// Note: This check is racy. If you need to conditionally install the profiler,
/// call `install()` directly and handle `InstallError::AlreadyInstalled`.
pub fn is_installed() -> bool {
    ACTIVE.get().is_some()
}

/// Push a synthetic `RawAlloc` into the installed profiler's queue.
///
/// Returns `false` if the profiler is not installed or the queue is full.
/// Intended for integration tests that verify the source→trace pipeline.
#[cfg(feature = "test-util")]
pub fn push_test_alloc(addr: u64, size: u64, ts_ns: u64) -> bool {
    let Some(inner) = ACTIVE.get() else {
        return false;
    };
    let mut frames = [0u64; DEFAULT_MAX_FRAMES];
    frames[0] = 0xDEAD;
    frames[1] = 0xBEEF;
    let raw = RawAlloc {
        tid: crate::memory_profiling::tid::current_tid(),
        size,
        addr,
        ts_ns,
        frames,
        frame_count: 2,
    };
    inner.rings.alloc_queue.push(raw).is_ok()
}

/// Errors that can occur during [`MemoryProfiler::install`].
#[derive(Debug)]
#[non_exhaustive]
pub enum InstallError {
    /// `install()` was already called once for this process.
    AlreadyInstalled,
    /// The SIGSEGV handler used by the FP unwinder failed to install.
    Unwinder(std::io::Error),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyInstalled => {
                f.write_str("memory profiler already installed in this process")
            }
            Self::Unwinder(e) => write!(f, "failed to install SIGSEGV handler for unwinder: {e}"),
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AlreadyInstalled => None,
            Self::Unwinder(e) => Some(e),
        }
    }
}

/// Memory profiler entry point.
///
/// Build with `MemoryProfiler::from_config(cfg)` or
/// `MemoryProfiler::with_defaults()`, then [`install`](Self::install) it. Set
/// a [`SamplingAllocator`](crate::memory_profiling::SamplingAllocator) as the global
/// allocator for the hook to fire.
///
/// Install is permanent for the life of the process (the allocator hook reads
/// a process-global). Draining stops when the returned [`MemorySampler`] is
/// dropped.
#[derive(Debug)]
pub struct MemoryProfiler {
    config: MemoryProfilingConfig,
}

use crate::memory_profiling::config::MemoryProfilingConfig;

impl MemoryProfiler {
    /// Build a profiler from a [`MemoryProfilingConfig`].
    pub fn from_config(config: MemoryProfilingConfig) -> Self {
        Self { config }
    }

    /// Build a profiler with default configuration.
    pub fn with_defaults() -> Self {
        Self::from_config(MemoryProfilingConfig::default())
    }

    /// Install the profiler process-wide and return a [`MemorySampler`] that
    /// drains captured allocations.
    ///
    /// Standalone consumers call [`MemorySampler::drain`] on a cadence. The
    /// dial9 integration (the `dial9-source` feature) wraps the sampler in a
    /// `Source` instead.
    pub fn install(self) -> Result<MemorySampler, InstallError> {
        let unwinder = Unwinder::install().map_err(InstallError::Unwinder)?;

        let rings = Arc::new(RingBuffers::new(
            self.config.ring_capacity(),
            // Free queue is sized 8× the alloc queue — see
            // `DEFAULT_FREE_QUEUE_CAPACITY` in `ring.rs` for the rationale.
            self.config.ring_capacity() * 8,
        ));

        let liveset = if self.config.track_liveset() {
            Some(Arc::new(scc::HashIndex::with_capacity_and_hasher(
                0,
                dial9_trace_format::encoder::FxBuildHasher::default(),
            )))
        } else {
            None
        };
        // Clone the Arc for the consumer (`MemorySampler`) so it can service
        // shutdown-flagged frees by doing the liveset peek/remove on its own
        // healthy thread. See `MemorySampler::drain`.
        let consumer_liveset = liveset.as_ref().map(Arc::clone);
        let sample_rate_bytes = self.config.sample_rate_bytes();

        let inner = MemoryProfilerInner {
            unwinder,
            rings: Arc::clone(&rings),
            sample_rate_bytes,
            liveset,
            rng_seed: self.config.rng_seed(),
        };

        ACTIVE
            .set(inner)
            .map_err(|_| InstallError::AlreadyInstalled)?;

        Ok(MemorySampler {
            rings,
            liveset: consumer_liveset,
            sample_rate_bytes,
            prev_dropped_allocs: 0,
            prev_dropped_frees: 0,
        })
    }
}

/// dial9 integration (the `dial9-source` feature).
#[cfg(feature = "dial9-source")]
impl MemoryProfiler {
    /// Install the profiler and register its
    /// [`MemoryProfileSource`](crate::memory_profiling::MemoryProfileSource)
    /// with the given dial9 session, so sampled allocations flow into the
    /// trace. The source drains for the life of the session.
    ///
    /// No-op on a disabled handle (the allocator hook stays short-circuited).
    pub fn install_into(
        self,
        handle: &dial9_core::handle::Dial9Handle,
    ) -> Result<(), InstallError> {
        if !handle.is_enabled() {
            return Ok(());
        }
        let sampler = self.install()?;
        let source = crate::memory_profiling::source::MemoryProfileSource::new(sampler);
        if let Some(shared) = handle.shared() {
            shared.push_source(Box::new(source));
        }
        Ok(())
    }
}

/// A captured allocation or free, yielded by [`MemorySampler::drain`].
#[derive(Debug)]
#[non_exhaustive]
pub enum MemorySample<'a> {
    /// A sampled allocation.
    Alloc(AllocSample<'a>),
    /// A free paired with a previously-sampled allocation.
    Free(FreeSample),
}

/// A sampled allocation.
#[derive(Debug)]
#[non_exhaustive]
pub struct AllocSample<'a> {
    /// OS thread ID of the allocating thread.
    pub tid: u32,
    /// Allocation size in bytes (the requested size).
    pub size: u64,
    /// Returned pointer.
    pub addr: u64,
    /// Monotonic-ns timestamp of the allocation.
    pub timestamp_ns: u64,
    /// Captured stack, most-recent caller first.
    pub callchain: &'a [u64],
}

/// A free paired with a previously-sampled allocation. Only produced when
/// liveset tracking is enabled.
#[derive(Debug)]
#[non_exhaustive]
pub struct FreeSample {
    /// OS thread ID of the freeing thread.
    pub tid: u32,
    /// Pointer that was freed.
    pub addr: u64,
    /// Size of the original allocation (denormalized from the liveset).
    pub size: u64,
    /// Monotonic-ns timestamp of the original allocation.
    pub alloc_timestamp_ns: u64,
    /// Monotonic-ns timestamp of the free.
    pub timestamp_ns: u64,
}

/// Ring-overflow counts since the previous [`MemorySampler::take_dropped`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct DroppedSamples {
    /// Alloc samples dropped to alloc-queue overflow.
    pub allocs: u64,
    /// Free samples dropped to free-queue overflow.
    pub frees: u64,
}

/// Consumer side of the memory profiler. Returned by
/// [`MemoryProfiler::install`].
///
/// Call [`drain`](Self::drain) on a cadence to pull captured allocations and
/// frees, and [`take_dropped`](Self::take_dropped) to observe ring overflow.
///
/// With the producer-side liveset, every non-shutdown free that reaches the
/// queue is guaranteed to correspond to a previously-sampled allocation: the
/// producer already did the peek/remove and denormalized `size` and
/// `alloc_ts_ns` onto the record. Shutdown-flagged frees (pushed by a thread
/// in TLS teardown that couldn't safely peek the liveset) are resolved here
/// against the shared liveset, with a race check that skips the free when the
/// address was reused before this drain.
pub struct MemorySampler {
    rings: Arc<RingBuffers>,
    liveset: Option<Arc<Liveset>>,
    sample_rate_bytes: u64,
    prev_dropped_allocs: u64,
    prev_dropped_frees: u64,
}

impl std::fmt::Debug for MemorySampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySampler").finish_non_exhaustive()
    }
}

impl MemorySampler {
    /// The configured mean bytes-between-samples. Useful as segment metadata.
    pub fn sample_rate_bytes(&self) -> u64 {
        self.sample_rate_bytes
    }

    /// Drain all pending samples, best-effort timestamp-ordered.
    ///
    /// Merge-sorts the alloc and free queues by timestamp. Ordering is not
    /// guaranteed perfect — multiple producers push concurrently — but it is
    /// good enough for profiling. Anything a producer pushes mid-drain has a
    /// timestamp later than what's already been emitted, so it is picked up
    /// this cycle or the next.
    pub fn drain(&mut self, mut f: impl FnMut(MemorySample<'_>)) {
        // Hold one peeked element from each queue and emit the older one.
        // `crossbeam_queue::ArrayQueue` has no peek API, so we pop into local
        // slots and only refill after we emit.
        let mut next_alloc: Option<RawAlloc> = self.rings.alloc_queue.pop();
        let mut next_free: Option<RawFree> = self.rings.free_queue.pop();
        loop {
            match (&next_alloc, &next_free) {
                (None, None) => break,
                (Some(_), None) => {
                    let a = next_alloc.take().expect("checked Some above");
                    self.emit_alloc(a, &mut f);
                    next_alloc = self.rings.alloc_queue.pop();
                }
                (None, Some(_)) => {
                    let fr = next_free.take().expect("checked Some above");
                    self.emit_free(fr, &mut f);
                    next_free = self.rings.free_queue.pop();
                }
                (Some(a), Some(fr)) => {
                    if a.ts_ns <= fr.ts_ns {
                        let a = next_alloc.take().expect("checked Some above");
                        self.emit_alloc(a, &mut f);
                        next_alloc = self.rings.alloc_queue.pop();
                    } else {
                        let fr = next_free.take().expect("checked Some above");
                        self.emit_free(fr, &mut f);
                        next_free = self.rings.free_queue.pop();
                    }
                }
            }
        }
    }

    fn emit_alloc(&self, a: RawAlloc, f: &mut impl FnMut(MemorySample<'_>)) {
        let count = (a.frame_count as usize).min(DEFAULT_MAX_FRAMES);
        f(MemorySample::Alloc(AllocSample {
            tid: a.tid,
            size: a.size,
            addr: a.addr,
            timestamp_ns: a.ts_ns,
            callchain: &a.frames[..count],
        }));
    }

    fn emit_free(&self, fr: RawFree, f: &mut impl FnMut(MemorySample<'_>)) {
        // No liveset means the producer drops every free; nothing to emit.
        let Some(liveset) = &self.liveset else {
            return;
        };

        // Resolve `(size, alloc_ts_ns)`:
        // - **Normal frees** carry denormalized data from the producer's
        //   peek/remove (see `hook::on_dealloc`); use it directly.
        // - **Shutdown-flagged frees** were pushed by a thread in TLS
        //   teardown that couldn't safely touch `scc`. Do the peek/remove
        //   here, with a race check: if the entry's `alloc_ts_ns` is greater
        //   than or equal to `fr.ts_ns`, an address-reuse race (or timestamp
        //   tie on nearby cores) means the entry now belongs to a *later*
        //   allocation. Leave it; the new alloc's eventual non-shutdown free
        //   will emit the correct event.
        let (size, alloc_ts_ns) = if fr.shutdown {
            let Some((size, alloc_ts_ns)) = liveset.peek_with(&fr.addr, |_, v| *v) else {
                return; // already cleaned, or never sampled
            };
            if alloc_ts_ns >= fr.ts_ns {
                return; // address-reuse race or timestamp tie — see comment above
            }
            liveset.remove(&fr.addr);
            (size, alloc_ts_ns)
        } else {
            (fr.size, fr.alloc_ts_ns)
        };

        f(MemorySample::Free(FreeSample {
            tid: fr.tid,
            addr: fr.addr,
            size,
            alloc_timestamp_ns: alloc_ts_ns,
            timestamp_ns: fr.ts_ns,
        }));
    }

    /// Ring-overflow deltas since the last call (new drops since the previous
    /// `take_dropped`). Relaxed loads: the consumer is the sole reader and
    /// only needs eventual visibility of producer increments.
    pub fn take_dropped(&mut self) -> DroppedSamples {
        let current_allocs = self.rings.dropped_allocs.load(Ordering::Relaxed);
        let current_frees = self.rings.dropped_frees.load(Ordering::Relaxed);
        let dropped = DroppedSamples {
            allocs: current_allocs.saturating_sub(self.prev_dropped_allocs),
            frees: current_frees.saturating_sub(self.prev_dropped_frees),
        };
        self.prev_dropped_allocs = current_allocs;
        self.prev_dropped_frees = current_frees;
        dropped
    }
}

#[cfg(test)]
impl MemorySampler {
    /// Build a sampler over arbitrary rings for tests, optionally with a fresh
    /// liveset (enabling free emission).
    pub(crate) fn new_for_test(
        rings: Arc<RingBuffers>,
        track_liveset: bool,
        sample_rate_bytes: u64,
    ) -> Self {
        let liveset = track_liveset.then(|| {
            Arc::new(scc::HashIndex::with_capacity_and_hasher(
                0,
                dial9_trace_format::encoder::FxBuildHasher::default(),
            ))
        });
        Self {
            rings,
            liveset,
            sample_rate_bytes,
            prev_dropped_allocs: 0,
            prev_dropped_frees: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum Got {
        Alloc {
            tid: u32,
            size: u64,
            addr: u64,
            ts: u64,
            cc: Vec<u64>,
        },
        Free {
            tid: u32,
            addr: u64,
            size: u64,
            alloc_ts: u64,
            ts: u64,
        },
    }

    fn collect(sampler: &mut MemorySampler) -> Vec<Got> {
        let mut out = Vec::new();
        sampler.drain(|s| {
            out.push(match s {
                MemorySample::Alloc(a) => Got::Alloc {
                    tid: a.tid,
                    size: a.size,
                    addr: a.addr,
                    ts: a.timestamp_ns,
                    cc: a.callchain.to_vec(),
                },
                MemorySample::Free(f) => Got::Free {
                    tid: f.tid,
                    addr: f.addr,
                    size: f.size,
                    alloc_ts: f.alloc_timestamp_ns,
                    ts: f.timestamp_ns,
                },
            })
        });
        out
    }

    fn rings(alloc_cap: usize, free_cap: usize) -> Arc<RingBuffers> {
        Arc::new(RingBuffers::new(alloc_cap, free_cap))
    }

    fn fresh_liveset() -> Arc<Liveset> {
        Arc::new(scc::HashIndex::with_capacity_and_hasher(
            0,
            dial9_trace_format::encoder::FxBuildHasher::default(),
        ))
    }

    fn sampler_with(rings: Arc<RingBuffers>, liveset: Option<Arc<Liveset>>) -> MemorySampler {
        MemorySampler {
            rings,
            liveset,
            sample_rate_bytes: 512 * 1024,
            prev_dropped_allocs: 0,
            prev_dropped_frees: 0,
        }
    }

    fn make_raw_alloc(addr: u64, size: u64, ts_ns: u64) -> RawAlloc {
        let mut frames = [0u64; DEFAULT_MAX_FRAMES];
        frames[0] = 0xAAAA;
        frames[1] = 0xBBBB;
        frames[2] = 0xCCCC;
        RawAlloc {
            tid: 1,
            size,
            addr,
            ts_ns,
            frames,
            frame_count: 3,
        }
    }

    fn make_raw_free(addr: u64, ts_ns: u64, size: u64, alloc_ts_ns: u64) -> RawFree {
        RawFree {
            tid: 2,
            addr,
            ts_ns,
            size,
            alloc_ts_ns,
            shutdown: false,
        }
    }

    fn make_shutdown_free(addr: u64, ts_ns: u64, tid: u32) -> RawFree {
        RawFree {
            tid,
            addr,
            ts_ns,
            size: 0,
            alloc_ts_ns: 0,
            shutdown: true,
        }
    }

    #[test]
    fn drain_emits_alloc() {
        let r = rings(16, 16);
        r.alloc_queue.push(make_raw_alloc(0x1000, 4096, 100)).ok();
        let mut s = sampler_with(Arc::clone(&r), None);
        assert_eq!(
            collect(&mut s),
            vec![Got::Alloc {
                tid: 1,
                size: 4096,
                addr: 0x1000,
                ts: 100,
                cc: vec![0xAAAA, 0xBBBB, 0xCCCC],
            }]
        );
    }

    #[test]
    fn drain_emits_free_for_matching_alloc() {
        let r = rings(16, 16);
        r.alloc_queue.push(make_raw_alloc(0x2000, 512, 200)).ok();
        r.free_queue.push(make_raw_free(0x2000, 300, 512, 200)).ok();
        let mut s = sampler_with(Arc::clone(&r), Some(fresh_liveset()));
        let got = collect(&mut s);
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[1],
            Got::Free {
                tid: 2,
                addr: 0x2000,
                size: 512,
                alloc_ts: 200,
                ts: 300,
            }
        );
    }

    /// Any free in the queue is emitted (the producer-side liveset already
    /// filtered the unsampled ones).
    #[test]
    fn free_in_queue_is_always_emitted() {
        let r = rings(16, 16);
        r.free_queue.push(make_raw_free(0x9999, 400, 128, 100)).ok();
        let mut s = sampler_with(Arc::clone(&r), Some(fresh_liveset()));
        let got = collect(&mut s);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0],
            Got::Free {
                tid: 2,
                addr: 0x9999,
                size: 128,
                alloc_ts: 100,
                ts: 400,
            }
        );
    }

    #[test]
    fn liveset_off_drops_all_frees() {
        let r = rings(16, 16);
        r.alloc_queue.push(make_raw_alloc(0x3000, 128, 500)).ok();
        r.free_queue.push(make_raw_free(0x3000, 600, 128, 500)).ok();
        let mut s = sampler_with(Arc::clone(&r), None);
        let got = collect(&mut s);
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], Got::Alloc { .. }));
    }

    #[test]
    fn alloc_then_free_in_separate_drains() {
        let r = rings(16, 16);
        let mut s = sampler_with(Arc::clone(&r), Some(fresh_liveset()));

        r.alloc_queue.push(make_raw_alloc(0x4000, 256, 700)).ok();
        let got = collect(&mut s);
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], Got::Alloc { .. }));

        r.free_queue.push(make_raw_free(0x4000, 800, 256, 700)).ok();
        let got = collect(&mut s);
        assert_eq!(
            got,
            vec![Got::Free {
                tid: 2,
                addr: 0x4000,
                size: 256,
                alloc_ts: 700,
                ts: 800,
            }]
        );
    }

    /// Two allocs + one free at the same address resolve atomically via the
    /// producer-side liveset; the merge keeps timestamp order.
    #[test]
    fn address_reuse_emits_correct_events() {
        let r = rings(16, 16);
        r.alloc_queue.push(make_raw_alloc(0x5000, 256, 100)).ok();
        r.free_queue.push(make_raw_free(0x5000, 200, 256, 100)).ok();
        r.alloc_queue.push(make_raw_alloc(0x5000, 512, 300)).ok();
        let mut s = sampler_with(Arc::clone(&r), Some(fresh_liveset()));

        let got = collect(&mut s);
        let allocs = got
            .iter()
            .filter(|g| matches!(g, Got::Alloc { .. }))
            .count();
        let frees: Vec<_> = got
            .iter()
            .filter(|g| matches!(g, Got::Free { .. }))
            .collect();
        assert_eq!(allocs, 2);
        assert_eq!(frees.len(), 1);
        assert_eq!(
            *frees[0],
            Got::Free {
                tid: 2,
                addr: 0x5000,
                size: 256,
                alloc_ts: 100,
                ts: 200,
            }
        );
    }

    /// Realloc-shaped sequence with strictly increasing timestamps drains in
    /// order.
    #[test]
    fn realloc_ordering_emits_correct_frees() {
        let (t1, t2, t3, t4) = (100, 200, 300, 400);
        let r = rings(16, 16);
        r.alloc_queue.push(make_raw_alloc(0x6000, 256, t1)).ok();
        r.free_queue.push(make_raw_free(0x6000, t2, 256, t1)).ok();
        r.alloc_queue.push(make_raw_alloc(0x6000, 512, t3)).ok();
        let mut s = sampler_with(Arc::clone(&r), Some(fresh_liveset()));

        let got = collect(&mut s);
        let frees: Vec<_> = got
            .iter()
            .filter(|g| matches!(g, Got::Free { .. }))
            .collect();
        assert_eq!(frees.len(), 1);
        assert_eq!(
            *frees[0],
            Got::Free {
                tid: 2,
                addr: 0x6000,
                size: 256,
                alloc_ts: t1,
                ts: t2,
            }
        );

        r.free_queue.push(make_raw_free(0x6000, t4, 512, t3)).ok();
        let got2 = collect(&mut s);
        assert_eq!(
            got2,
            vec![Got::Free {
                tid: 2,
                addr: 0x6000,
                size: 512,
                alloc_ts: t3,
                ts: t4,
            }]
        );
    }

    /// Happy-path shutdown drain: the consolidator peeks the liveset, emits a
    /// correct free, and removes the entry.
    #[test]
    fn shutdown_drain_emits_free_and_cleans_liveset() {
        let r = rings(16, 16);
        let liveset = fresh_liveset();
        liveset.insert(0xAABB, (4096, 100)).expect("insert");
        r.free_queue.push(make_shutdown_free(0xAABB, 200, 7)).ok();
        let mut s = sampler_with(Arc::clone(&r), Some(Arc::clone(&liveset)));

        assert_eq!(
            collect(&mut s),
            vec![Got::Free {
                tid: 7,
                addr: 0xAABB,
                size: 4096,
                alloc_ts: 100,
                ts: 200,
            }]
        );
        assert!(liveset.peek_with(&0xAABB, |_, _| ()).is_none());
    }

    /// Address reused for a newer allocation before the shutdown free drains:
    /// detected via `alloc_ts >= free.ts`, entry left intact, no free emitted.
    #[test]
    fn shutdown_drain_detects_address_reuse_race() {
        let r = rings(16, 16);
        let liveset = fresh_liveset();
        liveset.insert(0xAABB, (8192, 300)).expect("insert");
        r.free_queue.push(make_shutdown_free(0xAABB, 100, 9)).ok();
        let mut s = sampler_with(Arc::clone(&r), Some(Arc::clone(&liveset)));

        assert!(collect(&mut s).is_empty());
        assert_eq!(liveset.peek_with(&0xAABB, |_, v| *v), Some((8192, 300)));
    }

    #[test]
    fn shutdown_drain_ignores_misses() {
        let r = rings(16, 16);
        let liveset = fresh_liveset();
        r.free_queue.push(make_shutdown_free(0xDEAD, 100, 1)).ok();
        let mut s = sampler_with(Arc::clone(&r), Some(liveset));
        assert!(collect(&mut s).is_empty());
    }

    #[test]
    fn shutdown_drain_skips_on_timestamp_tie() {
        let r = rings(16, 16);
        let liveset = fresh_liveset();
        liveset.insert(0xBBCC, (2048, 100)).expect("insert");
        r.free_queue.push(make_shutdown_free(0xBBCC, 100, 3)).ok();
        let mut s = sampler_with(Arc::clone(&r), Some(Arc::clone(&liveset)));

        assert!(collect(&mut s).is_empty());
        assert_eq!(liveset.peek_with(&0xBBCC, |_, v| *v), Some((2048, 100)));
    }

    #[test]
    fn take_dropped_reports_deltas() {
        let r = rings(1, 1);
        let mut s = sampler_with(Arc::clone(&r), None);
        // Overflow both queues: cap 1, push 3 → 2 dropped each.
        for i in 0..3 {
            r.push_alloc(make_raw_alloc(0x10 + i, 64, i));
            r.push_free(make_raw_free(0x10 + i, i, 64, 0));
        }
        let d = s.take_dropped();
        assert_eq!(d.allocs, 2);
        assert_eq!(d.frees, 2);
        // Second call sees no new drops.
        let d2 = s.take_dropped();
        assert_eq!(d2.allocs, 0);
        assert_eq!(d2.frees, 0);
    }
}
