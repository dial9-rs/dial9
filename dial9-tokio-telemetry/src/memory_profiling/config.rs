//! `MemoryProfilingConfig` and the `TimestampMode` enum (design §8).

/// Default mean bytes-between-samples — geometric sampling rate.
///
/// At 512 KiB, a service doing 1 GB/s of allocation generates ~2000
/// samples/sec — plenty of signal, trivial overhead. See design §1.
pub const DEFAULT_SAMPLE_RATE_BYTES: u64 = 512 * 1024;

/// Default number of `RawAlloc` slots in the producer-to-consolidator
/// alloc ring. See design §5 "Sizing the ring".
pub const DEFAULT_RING_CAPACITY: usize = 4096;

/// How `AllocEvent.timestamp_ns` is populated.
///
/// See design §8 for the full motivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TimestampMode {
    /// Reuse the timestamp from the most recent `PollStart` on this thread
    /// (~2 ns TLS load). Falls back to `clock_monotonic_ns()` when called
    /// outside a task poll (e.g., between polls, or on non-worker threads).
    #[default]
    ReusePollStart,

    /// Emit events with `timestamp_ns = 0`. Smallest on-disk size.
    None,

    /// Call `clock_monotonic_ns()` per sampled allocation (~25 ns via vDSO).
    Precise,
}

/// Configuration for the memory profiler.
///
/// Built via `MemoryProfilingConfig::builder()...build()`. See design §8.
#[derive(Debug, Clone, bon::Builder)]
#[non_exhaustive]
pub struct MemoryProfilingConfig {
    /// Mean bytes between sampled allocations. Default 512 KiB.
    #[builder(default = DEFAULT_SAMPLE_RATE_BYTES)]
    sample_rate_bytes: u64,

    /// Whether to track the liveset for leak detection. Default `false`.
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
