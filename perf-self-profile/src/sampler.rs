//! Shared types for the sampler: event source, configuration, and sample data.

/// Which event source to sample on.
// TODO: these variants are currently Linux-specific (perf_event_open constants),
// consider cfg-gating individual variants when adding other platform backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventSource {
    /// `PERF_COUNT_HW_CPU_CYCLES` — hardware CPU cycle counter.
    /// Most precise, but may fail in VMs or containers without PMU access.
    HwCpuCycles,
    /// `PERF_COUNT_SW_CPU_CLOCK` — software hrtimer-based CPU clock.
    /// Works everywhere, slightly less precise.
    SwCpuClock,
    /// `PERF_COUNT_SW_TASK_CLOCK` — software task clock (per-thread CPU time).
    SwTaskClock,
    /// `PERF_COUNT_SW_CONTEXT_SWITCHES` — fires on every context switch.
    /// Captures the stack at the moment the thread is descheduled, revealing
    /// what code path led to the thread going off-CPU (e.g. mutex, I/O, preemption).
    SwContextSwitches,
    /// A kernel tracepoint, identified by its tracepoint ID.
    ///
    /// The ID comes from `/sys/kernel/debug/tracing/events/<subsystem>/<event>/id`.
    /// Samples include raw tracepoint data accessible via [`Sample::raw`].
    Tracepoint(u32),
}

/// The sampling mode to use.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum SamplingMode {
    /// Sample at this frequency in Hz (e.g., 999 or 4000).
    FrequencyHz(u64),
    /// Record one sample per this many events. `1` = every event.
    Period(u64),
}

/// Configuration for the sampler.
#[derive(Debug, Clone)]
pub struct SamplerConfig {
    pub(crate) event_source: EventSource,
    pub(crate) sampling: SamplingMode,
    pub(crate) include_kernel: bool,
    /// Maximum number of threads that can be tracked simultaneously in
    /// per-thread mode. Prevents unbounded fd/mmap growth if cleanup fails.
    /// Default: 256.
    pub(crate) max_tracked_threads: usize,
    /// Requested data size of each perf ring buffer, in bytes.
    pub(crate) ring_buffer_bytes: usize,
}

/// Default perf ring buffer size: with its metadata page at 4 KiB pages,
/// exactly the 516 KiB one CPU contributes to the default
/// `kernel.perf_event_mlock_kb` budget.
pub(crate) const DEFAULT_RING_BUFFER_BYTES: usize = 512 * 1024;

impl Default for SamplerConfig {
    fn default() -> Self {
        SamplerConfig {
            sampling: SamplingMode::FrequencyHz(999),
            event_source: EventSource::SwCpuClock,
            include_kernel: false,
            max_tracked_threads: 256,
            ring_buffer_bytes: DEFAULT_RING_BUFFER_BYTES,
        }
    }
}

impl SamplerConfig {
    /// Set the event source to sample on.
    pub fn event_source(mut self, source: EventSource) -> Self {
        self.event_source = source;
        self
    }

    /// Set the sampling mode (frequency or fixed period).
    pub fn sampling(mut self, mode: SamplingMode) -> Self {
        self.sampling = mode;
        self
    }

    /// Whether to include kernel stack frames.
    /// Requires `perf_event_paranoid` <= 1 (or CAP_PERFMON).
    pub fn include_kernel(mut self, yes: bool) -> Self {
        self.include_kernel = yes;
        self
    }

    /// Maximum number of threads tracked simultaneously in per-thread mode.
    /// If the cap is reached, `track_current_thread` returns an error instead
    /// of opening another fd. Default: 256.
    pub fn max_tracked_threads(mut self, max: usize) -> Self {
        self.max_tracked_threads = max;
        self
    }

    /// Data size of each perf ring buffer, in bytes, rounded up to a power of
    /// two pages. Must not be 0. Default: 512 KiB.
    ///
    /// One ring is mapped per CPU in process-wide mode and per tracked thread
    /// in per-thread mode. Unprivileged rings count against the perf mlock
    /// budget (`kernel.perf_event_mlock_kb` per CPU plus `RLIMIT_MEMLOCK`).
    /// A ring that fills before it is drained drops samples; see
    /// [`PerfSampler::take_lost_samples`](crate::PerfSampler::take_lost_samples).
    ///
    /// Ignored by the ctimer backend.
    pub fn ring_buffer_size(mut self, bytes: usize) -> Self {
        self.ring_buffer_bytes = bytes;
        self
    }

    /// Reject settings no backend accepts, so a bad config fails the same way
    /// whichever backend ends up running.
    #[cfg_attr(
        not(any(
            target_os = "linux",
            all(target_os = "android", target_arch = "aarch64")
        )),
        allow(dead_code)
    )]
    pub(crate) fn validate(&self) -> std::io::Result<()> {
        if self.ring_buffer_bytes == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "perf ring buffer size must be >= 1 byte",
            ));
        }
        Ok(())
    }
}

/// A single sample captured from perf events.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Sample {
    /// Instruction pointer at the time of the sample.
    pub ip: u64,
    /// Process ID.
    pub pid: u32,
    /// Thread ID.
    pub tid: u32,
    /// Timestamp in nanoseconds from `CLOCK_MONOTONIC` (set via `use_clockid`).
    pub time: u64,
    /// CPU the sample was taken on, if the backend could determine it.
    ///
    /// Perf-based sampling always fills this in (via `PERF_SAMPLE_CPU`).
    /// The ctimer fallback sets it to `None` when `getcpu` fails.
    pub cpu: Option<u32>,
    /// The actual period for this sample.
    pub period: u64,
    /// Stack frames from the callchain.
    /// First entry is the instruction pointer (leaf), rest are return addresses minus one.
    /// Kernel context markers and hypervisor frames are filtered out.
    pub callchain: Vec<u64>,
    /// Raw tracepoint data, present only for [`EventSource::Tracepoint`] events.
    /// Parse with [`TracepointDef::extract_fields`](crate::tracepoint::TracepointDef::extract_fields).
    pub raw: Option<Vec<u8>>,
}

/// CPU sampling backend a [`PerfSampler`](crate::PerfSampler) runs on, after
/// `Auto` resolves.
///
/// Variants are `#[non_exhaustive]` so they can gain fields: match them as
/// `ActiveCpuBackend::Ctimer { .. }`, or compare [`as_str`](Self::as_str).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActiveCpuBackend {
    /// `perf_event_open`. A `CpuProfiler` on it samples every thread
    /// descended from the one that started it.
    #[non_exhaustive]
    Perf,
    /// Per-thread CPU timers. A `CpuProfiler` on it samples only threads dial9
    /// tracks and, on Linux, the thread that started it.
    #[non_exhaustive]
    Ctimer,
}

impl ActiveCpuBackend {
    /// `"perf"` or `"ctimer"`, as written to segment metadata.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Perf => "perf",
            Self::Ctimer => "ctimer",
        }
    }
}
