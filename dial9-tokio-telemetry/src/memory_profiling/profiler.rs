//! `MemoryProfiler::install()` — the install-once entry point.

use crate::memory_profiling::config::MemoryProfilingConfig;
use crate::memory_profiling::ring::RingBuffers;
#[cfg(feature = "analysis")]
use crate::memory_profiling::ring::{DEFAULT_MAX_FRAMES, RawAlloc};

use crate::memory_profiling::source::MemoryProfileSource;
use crate::telemetry::recorder::Dial9Handle;
use dial9_perf_self_profile::unwinder::Unwinder;
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
    /// Held to prevent `SharedState` from being dropped while the
    /// profiler is active. Never read after construction; the value
    /// matters, not its accesses.
    #[expect(
        dead_code,
        reason = "lifetime hold for SharedState; the field's existence is the contract"
    )]
    pub(crate) handle: Dial9Handle,
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
#[cfg(feature = "analysis")]
#[doc(hidden)]
pub fn push_test_alloc(addr: u64, size: u64, ts_ns: u64) -> bool {
    let Some(inner) = ACTIVE.get() else {
        return false;
    };
    let mut frames = [0u64; DEFAULT_MAX_FRAMES];
    frames[0] = 0xDEAD;
    frames[1] = 0xBEEF;
    let raw = RawAlloc {
        tid: crate::telemetry::events::current_tid(),
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
/// Use `MemoryProfiler::from_config(cfg).install(handle)` or
/// `MemoryProfiler::with_defaults().install(handle)`.
///
/// Install is permanent for the life of the process.
#[derive(Debug)]
pub struct MemoryProfiler {
    config: MemoryProfilingConfig,
}

impl MemoryProfiler {
    /// Build a profiler from a [`MemoryProfilingConfig`].
    pub fn from_config(config: MemoryProfilingConfig) -> Self {
        Self { config }
    }

    /// Build a profiler with default configuration.
    pub fn with_defaults() -> Self {
        Self::from_config(MemoryProfilingConfig::default())
    }

    /// Install the profiler with the given handle.
    ///
    /// On a disabled handle, install is a no-op (returns `Ok` but does not
    /// publish state). `ACTIVE.get()` remains `None` so the allocator hook
    /// short-circuits.
    pub fn install(self, handle: Dial9Handle) -> Result<MemoryProfilerGuard, InstallError> {
        if !handle.is_enabled() {
            return Ok(MemoryProfilerGuard { _private: () });
        }

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
        // Clone the Arc for the consolidator (`MemoryProfileSource`) so it
        // can service shutdown-flagged frees by doing the liveset peek/remove
        // on its own healthy thread. See `source.rs::handle_free`.
        let source_liveset = liveset.as_ref().map(Arc::clone);

        let inner = MemoryProfilerInner {
            unwinder,
            handle: handle.clone(),
            rings: Arc::clone(&rings),
            sample_rate_bytes: self.config.sample_rate_bytes(),
            liveset,
            rng_seed: self.config.rng_seed(),
        };

        ACTIVE
            .set(inner)
            .map_err(|_| InstallError::AlreadyInstalled)?;

        let shared = handle.shared().expect("checked is_enabled above");
        let source =
            MemoryProfileSource::new(rings, source_liveset, self.config.sample_rate_bytes());
        shared.push_source(Box::new(source));

        Ok(MemoryProfilerGuard { _private: () })
    }
}

/// RAII guard returned by [`MemoryProfiler::install`].
///
/// Dropping does **NOT** uninstall the profiler — install is permanent.
#[must_use = "dropping the guard does not uninstall the profiler; bind it to keep the install lifetime explicit"]
pub struct MemoryProfilerGuard {
    pub(crate) _private: (),
}

impl std::fmt::Debug for MemoryProfilerGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryProfilerGuard")
            .finish_non_exhaustive()
    }
}
