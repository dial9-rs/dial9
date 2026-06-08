//! Configuration types for the memory profiler.

/// Default mean bytes-between-samples — geometric sampling rate.
///
/// At 512 KiB, a service doing 1 GB/s of allocation generates ~2000
/// samples/sec — plenty of signal, trivial overhead.
pub const DEFAULT_SAMPLE_RATE_BYTES: u64 = 512 * 1024;

/// Default number of slots in the producer-to-consolidator alloc ring.
pub const DEFAULT_RING_CAPACITY: usize = 4096;

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
    /// When enabled, a producer-side `scc::HashIndex` tracks sampled
    /// allocations. On dealloc, only addresses present in the liveset
    /// (i.e. previously sampled) produce a `RawFree` — ~99.9% of deallocs
    /// are filtered on the producer side with no queue contention.
    #[builder(default = false)]
    track_liveset: bool,

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
    pub fn build(self) -> MemoryProfilingConfig {
        let config = self.build_inner();
        assert!(
            config.sample_rate_bytes >= 1,
            "MemoryProfilingConfig::sample_rate_bytes must be >= 1; pass 1 for \
             'sample every allocation' mode"
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
    fn build_accepts_liveset() {
        let cfg = MemoryProfilingConfig::builder().track_liveset(true).build();
        assert!(cfg.track_liveset());
    }
}
