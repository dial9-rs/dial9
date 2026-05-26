//! Configuration types for the memory profiler.

/// Default mean bytes-between-samples — geometric sampling rate.
///
/// At 512 KiB, a service doing 1 GB/s of allocation generates ~2000
/// samples/sec — plenty of signal, trivial overhead.
pub const DEFAULT_SAMPLE_RATE_BYTES: u64 = 512 * 1024;

/// Default number of slots in the producer-to-consolidator alloc ring.
pub const DEFAULT_RING_CAPACITY: usize = 4096;

/// How `AllocEvent.timestamp_ns` is populated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TimestampMode {
    /// Reuse the timestamp from the most recent `PollStart` on this thread
    /// (~5 ns TLS load), falling back to `clock_monotonic_ns()` when called
    /// outside a task poll (e.g., between polls or on non-worker threads).
    ///
    /// In either case the returned timestamp is strictly greater than the
    /// previous one this thread observed (a 1 ns bump on ties), so events
    /// produced inside a single poll — including a free + alloc pair from
    /// in-place realloc — always have distinct, ordered timestamps. This
    /// is the default and the recommended mode when `track_liveset` is on.
    #[default]
    ReusePollStart,

    /// Emit events with `timestamp_ns = 0`. Smallest on-disk size.
    ///
    /// **Incompatible with `track_liveset(true)`** — the consolidator
    /// relies on timestamp ordering to disambiguate same-address
    /// free/alloc pairs (e.g. from in-place realloc). Use
    /// [`TimestampMode::ReusePollStart`] or [`TimestampMode::Precise`]
    /// instead when liveset tracking is on. The combination is rejected
    /// at config build time.
    None,

    /// Call `clock_monotonic_ns()` per sampled allocation (~25 ns via vDSO).
    Precise,
}

/// Configuration for the memory profiler.
///
/// Built via `MemoryProfilingConfig::builder()...build()`.
#[derive(Debug, Clone, bon::Builder)]
#[builder(finish_fn = build_inner)]
#[non_exhaustive]
pub struct MemoryProfilingConfig {
    /// Mean bytes between sampled allocations. Default 512 KiB.
    ///
    /// Lower values sample more allocations. **`sample_rate_bytes = 1`
    /// is a special "sample every allocation" mode**: every call to
    /// the allocator is recorded and the per-thread PRNG is bypassed
    /// entirely. `0` is rejected at build time — pass `1` for the
    /// "sample everything" semantics.
    ///
    /// # Going from sample sizes to estimated totals
    ///
    /// Each `Alloc` event in the trace carries the **raw size** of one
    /// sampled allocation. Summing raw sizes will undercount because
    /// only ~`s/R` allocations of size `s` are sampled. To recover
    /// unbiased totals, weight each sample by the inverse Poisson
    /// sampling probability:
    ///
    /// ```text
    /// total_bytes ≈ Σ s_i / (1 - exp(-s_i / R))
    /// total_count ≈ Σ   1 / (1 - exp(-s_i / R))
    /// ```
    ///
    /// where `R` is the `sample_rate_bytes` value above. The same
    /// formula handles all size regimes:
    ///
    /// - For `s << R`: each sample contributes ~`R` bytes (small
    ///   samples are scaled up).
    /// - For `s >> R`: each sample contributes ~`s` bytes (huge allocs
    ///   are sampled with probability ~1, no scaling needed).
    ///
    /// **Aggregate per sample, not per group.** When grouping by call
    /// site / task / type, weight each sample individually before
    /// summing. Sum-then-unbias under-reports skewed groups.
    ///
    /// See `docs/design/memory-profiling.md` for worked examples.
    #[builder(default = DEFAULT_SAMPLE_RATE_BYTES)]
    sample_rate_bytes: u64,

    /// Whether to track the liveset for leak detection. Default `false`.
    ///
    /// When enabled, a `RawFree` is pushed into the free queue on
    /// **every** deallocation (not just sampled ones). At very high
    /// dealloc rates the free queue can overflow; overflowed frees are
    /// silently dropped (counted in `dropped_frees`). A dropped free
    /// for a previously-sampled allocation means its liveset entry
    /// persists, inflating the reported live set until the next flush
    /// cycle or process exit. Size the `ring_capacity` accordingly for
    /// high-throughput services.
    #[builder(default = false)]
    track_liveset: bool,

    /// How `AllocEvent.timestamp_ns` is populated. See [`TimestampMode`].
    #[builder(default)]
    timestamp_mode: TimestampMode,

    /// Optional fixed seed for per-thread sampling PRNGs.
    rng_seed: Option<u64>,

    /// Number of slots in the alloc queue. Default 4096.
    /// The free queue is sized 8× this.
    #[builder(default = DEFAULT_RING_CAPACITY)]
    ring_capacity: usize,
}

impl<S: memory_profiling_config_builder::IsComplete> MemoryProfilingConfigBuilder<S> {
    /// Finalise the config.
    ///
    /// # Panics
    ///
    /// Panics if `sample_rate_bytes` was set to `0`. Use `1` for the
    /// "sample every allocation" mode — `0` would mean "zero bytes
    /// between samples", which is ambiguous (sample everything? sample
    /// nothing?), so we require the explicit `1`.
    ///
    /// Panics if `timestamp_mode == TimestampMode::None` is combined
    /// with `track_liveset(true)`. The liveset matches `Free` events to
    /// prior `Alloc` events by address, and relies on timestamp
    /// ordering to disambiguate same-address free/alloc pairs (e.g.
    /// in-place realloc). With every event at `ts = 0` the merge-sort
    /// drain in `MemoryProfileSource` cannot order those pairs and
    /// would corrupt the liveset.
    pub fn build(self) -> MemoryProfilingConfig {
        let config = self.build_inner();
        assert!(
            config.sample_rate_bytes >= 1,
            "MemoryProfilingConfig::sample_rate_bytes must be >= 1; pass 1 for \
             'sample every allocation' mode"
        );
        assert!(
            !(config.track_liveset && matches!(config.timestamp_mode, TimestampMode::None)),
            "MemoryProfilingConfig: TimestampMode::None is incompatible with \
             track_liveset(true). With every event at ts=0 the consolidator's \
             merge-sort drain cannot order same-address free/alloc pairs (e.g. \
             from in-place realloc) and would corrupt the liveset. Choose \
             TimestampMode::ReusePollStart (default) or TimestampMode::Precise \
             when liveset tracking is on."
        );
        config
    }
}

impl Default for MemoryProfilingConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl MemoryProfilingConfig {
    /// Mean bytes between sampled allocations.
    pub fn sample_rate_bytes(&self) -> u64 {
        self.sample_rate_bytes
    }
    /// Whether liveset tracking is enabled.
    pub fn track_liveset(&self) -> bool {
        self.track_liveset
    }
    /// Timestamp mode for alloc events.
    pub fn timestamp_mode(&self) -> TimestampMode {
        self.timestamp_mode
    }
    /// Optional fixed RNG seed.
    pub fn rng_seed(&self) -> Option<u64> {
        self.rng_seed
    }
    /// Ring capacity (alloc queue slots).
    pub fn ring_capacity(&self) -> usize {
        self.ring_capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_accepts_one() {
        let cfg = MemoryProfilingConfig::builder()
            .sample_rate_bytes(1)
            .build();
        assert_eq!(cfg.sample_rate_bytes(), 1);
    }

    #[test]
    fn build_accepts_default() {
        // Default uses DEFAULT_SAMPLE_RATE_BYTES = 512 KiB.
        let cfg = MemoryProfilingConfig::default();
        assert_eq!(cfg.sample_rate_bytes(), DEFAULT_SAMPLE_RATE_BYTES);
    }

    #[test]
    #[should_panic(expected = "sample_rate_bytes must be >= 1")]
    fn build_rejects_zero() {
        // `0` is ambiguous — pass `1` for "sample every allocation".
        let _ = MemoryProfilingConfig::builder()
            .sample_rate_bytes(0)
            .build();
    }

    #[test]
    #[should_panic(expected = "TimestampMode::None")]
    fn build_rejects_none_timestamp_with_liveset() {
        // Liveset tracking matches Free→Alloc by addr AND requires
        // timestamp ordering to disambiguate same-address free/alloc
        // pairs (e.g. in-place realloc). With every event at ts=0, the
        // merge-sort drain in `MemoryProfileSource` cannot tell which
        // came first — see PR #442 review.
        let _ = MemoryProfilingConfig::builder()
            .timestamp_mode(TimestampMode::None)
            .track_liveset(true)
            .build();
    }

    #[test]
    fn build_accepts_none_timestamp_without_liveset() {
        // The None+!liveset combo is fine: free events are dropped,
        // so there's no ordering hazard.
        let cfg = MemoryProfilingConfig::builder()
            .timestamp_mode(TimestampMode::None)
            .track_liveset(false)
            .build();
        assert!(matches!(cfg.timestamp_mode(), TimestampMode::None));
        assert!(!cfg.track_liveset());
    }

    #[test]
    fn build_accepts_liveset_with_reuse_poll_start() {
        // The intended liveset combo: ReusePollStart provides per-poll
        // timestamps, falling back to clock_monotonic_ns elsewhere.
        let cfg = MemoryProfilingConfig::builder()
            .timestamp_mode(TimestampMode::ReusePollStart)
            .track_liveset(true)
            .build();
        assert!(cfg.track_liveset());
    }

    #[test]
    fn build_accepts_liveset_with_precise() {
        // Precise also provides real timestamps.
        let cfg = MemoryProfilingConfig::builder()
            .timestamp_mode(TimestampMode::Precise)
            .track_liveset(true)
            .build();
        assert!(cfg.track_liveset());
    }
}
