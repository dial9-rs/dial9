//! CPU and scheduler profiling as a [`dial9_core::source::Source`].
//!
//! Provides two [`Source`] implementations that feed CPU stack samples
//! into the dial9 trace stream without any tokio dependency:
//!
//! - [`CpuProfiler`] — process-wide frequency-based CPU sampling.
//! - [`SchedProfiler`] — per-worker-thread context-switch capture.

use crate::{EventSource, PerfSampler, SamplerConfig, SamplingMode};
use dial9_core::buffer::{Encodable, ThreadLocalEncoder};
use dial9_core::source::{FlushContext, Source};
use dial9_trace_format::types::{EventEncoder, FieldType};
use dial9_trace_format::{InternedStackFrames, InternedString, TraceEvent, TraceField};
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;

// ── Wire sentinel ───────────────────────────────────────────────────────────

/// Worker ID sentinel used when the sample's worker is not yet known.
/// Attribution happens at analysis time via tid ↔ park/unpark mapping.
const WORKER_ID_UNKNOWN: u64 = 255;

// ── CpuSampleSource ─────────────────────────────────────────────────────────

/// What triggered a CPU sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuSampleSource {
    /// Periodic CPU profiling sample (frequency-based).
    CpuProfile = 0,
    /// Context switch captured by per-thread sched event tracking.
    SchedEvent = 1,
}

impl TraceField for CpuSampleSource {
    fn field_type() -> FieldType {
        FieldType::U8
    }
    fn encode<W: Write>(&self, enc: &mut EventEncoder<'_, W>) -> io::Result<()> {
        enc.write_u8(*self as u8)
    }
}

// ── CpuSampleEvent (wire format) ────────────────────────────────────────────

#[derive(TraceEvent)]
#[traceevent(wire_slot)]
struct CpuSampleEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    /// Worker ID on the wire. Always `WORKER_ID_UNKNOWN`; analysis resolves
    /// worker attribution via tid ↔ park/unpark mapping.
    worker_id: u64,
    tid: u32,
    source: CpuSampleSource,
    thread_name: Option<InternedString>,
    callchain: InternedStackFrames,
    /// CPU the sample was taken on, if the backend could determine it.
    ///
    /// Widened to `u64` on the wire so the field encodes as `OptionalVarint`:
    /// 1 byte when absent, typically 2 bytes (tag + small-varint) when present.
    cpu: Option<u64>,
}

// ── Internal types ──────────────────────────────────────────────────────────

/// Interned thread name, shared across drain calls so short-lived threads
/// are captured before `/proc/self/task/<tid>/comm` disappears.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThreadName(Arc<str>);

impl ThreadName {
    fn new(name: String) -> Self {
        Self(name.into())
    }

    fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

/// A raw CPU sample before worker-id resolution.
pub(crate) struct RawCpuSample {
    pub tid: u32,
    pub timestamp_nanos: u64,
    pub callchain: Vec<u64>,
    pub source: CpuSampleSource,
    pub cpu: Option<u32>,
}

/// Encodable wrapper for a raw sample. Interning of `thread_name` and
/// `callchain` happens in [`Encodable::encode`] against the thread-local
/// encoder's pools.
struct CpuSampleData {
    timestamp_nanos: u64,
    tid: u32,
    thread_name: Option<ThreadName>,
    source: CpuSampleSource,
    callchain: Vec<u64>,
    cpu: Option<u32>,
}

impl Encodable for CpuSampleData {
    fn encode(&self, enc: &mut ThreadLocalEncoder<'_>) {
        let thread_name = self
            .thread_name
            .as_ref()
            .map(|n| enc.intern_string(n.as_str()));
        let callchain = enc.intern_stack_frames(&self.callchain);
        enc.encode(&CpuSampleEvent {
            timestamp_ns: self.timestamp_nanos,
            worker_id: WORKER_ID_UNKNOWN,
            tid: self.tid,
            source: self.source,
            thread_name,
            callchain,
            cpu: self.cpu.map(u64::from),
        });
    }
}

// ── Platform helper ─────────────────────────────────────────────────────────

/// Read the thread name from `/proc/self/task/<tid>/comm`.
/// Returns `None` if the file can't be read.
pub(crate) fn read_thread_name(tid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ── Config types ────────────────────────────────────────────────────────────

/// Which CPU profiling backend to use.
///
/// The backend determines how stack samples are collected. Each variant only
/// exposes configuration knobs that the backend actually supports, making
/// invalid combinations (e.g. ctimer + kernel stacks) unrepresentable.
///
/// # `DIAL9_FORCE_CTIMER` interaction
///
/// The `DIAL9_FORCE_CTIMER` environment variable is only respected by
/// [`Auto`](CpuBackend::Auto). When [`Perf`](CpuBackend::Perf) or
/// [`Ctimer`](CpuBackend::Ctimer) is specified explicitly, the env var is
/// ignored — the caller has already made a deterministic choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CpuBackend {
    /// Try perf first; falls back to ctimer if `perf_event_open` is blocked.
    Auto,
    /// Perf backend via `perf_event_open`. Fails if perf is blocked.
    Perf,
    /// Ctimer backend (userspace frame-pointer unwinding via `SIGPROF`).
    Ctimer,
}

/// Configuration for CPU profiling integration.
///
/// # Examples
///
/// ```ignore
/// use dial9_perf_self_profile::{CpuProfilingConfig, EventSource};
///
/// // Default: try perf, fall back to ctimer:
/// let config = CpuProfilingConfig::default();
///
/// // Explicit perf with kernel stacks:
/// let config = CpuProfilingConfig::with_perf_backend()
///     .event_source(EventSource::SwCpuClock)
///     .include_kernel(true);
///
/// // Explicit ctimer:
/// let config = CpuProfilingConfig::with_ctimer_backend();
/// ```
#[derive(Debug, Clone)]
pub struct CpuProfilingConfig {
    frequency_hz: u64,
    backend: CpuBackend,
    event_source: EventSource,
    include_kernel: bool,
}

impl Default for CpuProfilingConfig {
    fn default() -> Self {
        Self {
            frequency_hz: 99,
            backend: CpuBackend::Auto,
            event_source: EventSource::SwCpuClock,
            include_kernel: false,
        }
    }
}

impl CpuProfilingConfig {
    // ── Constructors ────────────────────────────────────────────────────────

    /// Use the **perf** backend exclusively — no ctimer fallback.
    ///
    /// Fails at start time if `perf_event_open` is blocked. Use this when you
    /// need capabilities only perf can provide (kernel stacks, hardware
    /// counters, event-based sources).
    pub fn with_perf_backend() -> Self {
        Self {
            backend: CpuBackend::Perf,
            ..Self::default()
        }
    }

    /// Use the **ctimer** backend exclusively — no perf attempt.
    ///
    /// Avoids perf's per-CPU inherited context overhead entirely. Only supports
    /// frequency-based userspace CPU-time sampling (frame-pointer unwinding via
    /// `SIGPROF`). Cannot capture kernel stacks or hardware events.
    pub fn with_ctimer_backend() -> Self {
        Self {
            backend: CpuBackend::Ctimer,
            ..Self::default()
        }
    }

    // ── Setters ─────────────────────────────────────────────────────────────

    /// Sampling frequency in Hz. Default: 99 (low overhead).
    pub fn frequency_hz(mut self, hz: u64) -> Self {
        self.frequency_hz = hz;
        self
    }

    /// Which perf event source to sample on. Default: `SwCpuClock`.
    ///
    /// Ignored by [`Ctimer`](CpuBackend::Ctimer).
    pub fn event_source(mut self, source: EventSource) -> Self {
        self.event_source = source;
        self
    }

    /// Whether to include kernel stack frames. Default: `false`.
    ///
    /// Ignored by [`Ctimer`](CpuBackend::Ctimer).
    pub fn include_kernel(mut self, yes: bool) -> Self {
        self.include_kernel = yes;
        self
    }
}

/// Configuration for per-worker sched event capture (context switches).
///
/// Uses `perf_event_open` with `SwContextSwitches` in per-thread mode,
/// so each worker thread gets its own perf fd on first poll/park.
#[derive(Debug, Clone, Default)]
pub struct SchedEventConfig {
    sampling_interval: Option<u64>,
    include_kernel: bool,
}

impl SchedEventConfig {
    /// Record every Nth context switch. Default records every event.
    pub fn sampling_interval(mut self, n: u64) -> Self {
        self.sampling_interval = Some(n);
        self
    }

    /// Include kernel stack frames in callchains.
    pub fn include_kernel(mut self, yes: bool) -> Self {
        self.include_kernel = yes;
        self
    }
}

// ── CpuProfiler ─────────────────────────────────────────────────────────────

/// Process-wide CPU profiler. Registers a `perf_event_open` sampler and
/// drains raw stack traces into the trace stream on each flush cycle.
///
/// Worker attribution is left to analysis; each sample carries only its OS
/// `tid`, which the viewer maps to a worker via park/unpark events.
pub struct CpuProfiler {
    sampler: PerfSampler,
    pid: u32,
    /// OS tid → thread name, eagerly cached at drain time so short-lived
    /// threads are captured before they exit and their `comm` file disappears.
    tid_to_name: HashMap<u32, ThreadName>,
}

impl CpuProfiler {
    /// Start the process-wide CPU profiler with the given config.
    pub fn start(config: CpuProfilingConfig) -> io::Result<Self> {
        let sampler = match config.backend {
            CpuBackend::Auto => PerfSampler::start(
                SamplerConfig::default()
                    .event_source(config.event_source)
                    .sampling(SamplingMode::FrequencyHz(config.frequency_hz))
                    .include_kernel(config.include_kernel),
            )?,
            CpuBackend::Perf => PerfSampler::start_perf_only(
                SamplerConfig::default()
                    .event_source(config.event_source)
                    .sampling(SamplingMode::FrequencyHz(config.frequency_hz))
                    .include_kernel(config.include_kernel),
            )?,
            CpuBackend::Ctimer => PerfSampler::start_ctimer_only(
                SamplerConfig::default()
                    .sampling(SamplingMode::FrequencyHz(config.frequency_hz))
                    .include_kernel(false),
            )?,
        };
        Ok(Self {
            sampler,
            pid: std::process::id(),
            tid_to_name: HashMap::new(),
        })
    }

    /// Drain all pending perf samples as raw (tid, callchain) tuples.
    ///
    /// Filters out child-process samples (perf `inherit` leaks them).
    /// Eagerly caches thread names for non-worker tids.
    pub(crate) fn drain(&mut self, mut f: impl FnMut(RawCpuSample, Option<&ThreadName>)) {
        let pid = self.pid;
        self.sampler.for_each_sample(|sample| {
            if sample.pid != pid {
                return;
            }
            if !self.tid_to_name.contains_key(&sample.tid)
                && let Some(name) = read_thread_name(sample.tid)
            {
                self.tid_to_name.insert(sample.tid, ThreadName::new(name));
            }
            let thread_name = self.tid_to_name.get(&sample.tid);
            f(
                RawCpuSample {
                    tid: sample.tid,
                    timestamp_nanos: sample.time,
                    callchain: sample.callchain.clone(),
                    source: CpuSampleSource::CpuProfile,
                    cpu: sample.cpu,
                },
                thread_name,
            );
        });
    }
}

impl Source for CpuProfiler {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        self.drain(|raw, thread_name| {
            // worker_id is always UNKNOWN; analysis attributes via tid.
            ctx.record_event(&CpuSampleData {
                timestamp_nanos: raw.timestamp_nanos,
                tid: raw.tid,
                source: raw.source,
                callchain: raw.callchain,
                thread_name: thread_name.cloned(),
                cpu: raw.cpu,
            });
        });
    }

    fn name(&self) -> &'static str {
        "cpu_profile"
    }
}

// ── SchedProfiler ────────────────────────────────────────────────────────────

/// Per-thread sched event profiler. Captures context switches for each
/// worker thread that calls [`on_worker_thread_start`](Source::on_worker_thread_start).
pub struct SchedProfiler {
    sampler: PerfSampler,
}

impl SchedProfiler {
    /// Create a new sched profiler with the given config.
    pub fn new(config: SchedEventConfig) -> io::Result<Self> {
        let sampler = PerfSampler::new_per_thread(
            SamplerConfig::default()
                .event_source(EventSource::SwContextSwitches)
                .sampling(SamplingMode::Period(config.sampling_interval.unwrap_or(1)))
                .include_kernel(config.include_kernel),
        )?;
        Ok(Self { sampler })
    }

    pub(crate) fn track_current_thread(&mut self) -> io::Result<()> {
        self.sampler.track_current_thread()
    }

    pub(crate) fn stop_tracking_current_thread(&mut self) {
        self.sampler.stop_tracking_current_thread()
    }

    pub(crate) fn drain(&mut self, mut f: impl FnMut(RawCpuSample)) {
        self.sampler.for_each_sample(|sample| {
            f(RawCpuSample {
                tid: sample.tid,
                timestamp_nanos: sample.time,
                callchain: sample.callchain.clone(),
                source: CpuSampleSource::SchedEvent,
                cpu: sample.cpu,
            });
        });
    }
}

impl Source for SchedProfiler {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        self.drain(|raw| {
            // worker_id left UNKNOWN; attributed by tid at analysis.
            ctx.record_event(&CpuSampleData {
                timestamp_nanos: raw.timestamp_nanos,
                tid: raw.tid,
                source: raw.source,
                callchain: raw.callchain,
                thread_name: None,
                cpu: raw.cpu,
            });
        });
    }

    fn on_worker_thread_start(&mut self) -> io::Result<()> {
        self.track_current_thread()
    }

    fn on_thread_stop(&mut self) {
        self.stop_tracking_current_thread();
    }

    fn name(&self) -> &'static str {
        "sched"
    }
}

#[cfg(test)]
mod cpu_sample_round_trip_tests {
    use super::{CpuSampleEvent, CpuSampleSource, WORKER_ID_UNKNOWN};
    use dial9_trace_format::decoder::{DecodedFrame, Decoder};
    use dial9_trace_format::encoder::Encoder;
    use dial9_trace_format::types::FieldValue;

    /// Encode a `CpuSampleEvent` with the given `cpu` and decode it back to the
    /// event frame's `(timestamp, field values)`.
    fn round_trip(cpu: Option<u64>) -> (Option<u64>, Vec<FieldValue>) {
        let mut enc = Encoder::new();
        let thread_name = enc.intern_string("tokio-runtime-worker").unwrap();
        let callchain = enc
            .intern_stack_frames(&[0xdead_beef, 0xcafe_babe])
            .unwrap();
        enc.write(&CpuSampleEvent {
            timestamp_ns: 7_000_000,
            worker_id: WORKER_ID_UNKNOWN,
            tid: 9999,
            source: CpuSampleSource::CpuProfile,
            thread_name: Some(thread_name),
            callchain,
            cpu,
        })
        .unwrap();
        let bytes = enc.finish();

        Decoder::new(&bytes)
            .unwrap()
            .decode_all()
            .into_iter()
            .find_map(|frame| match frame {
                DecodedFrame::Event {
                    timestamp_ns,
                    values,
                    ..
                } => Some((timestamp_ns, values)),
                _ => None,
            })
            .expect("event frame")
    }

    #[test]
    fn cpu_sample_event_round_trips_with_cpu() {
        let (timestamp_ns, values) = round_trip(Some(3));
        assert_eq!(timestamp_ns, Some(7_000_000));
        assert_eq!(values[1], FieldValue::Varint(9999)); // tid
        // `cpu` is the last field; `Some(3)` encodes as an OptionalVarint.
        assert_eq!(*values.last().unwrap(), FieldValue::Varint(3));
    }

    #[test]
    fn cpu_sample_event_round_trips_without_cpu() {
        let (_timestamp_ns, values) = round_trip(None);
        // An absent `cpu` decodes as `FieldValue::None`.
        assert_eq!(*values.last().unwrap(), FieldValue::None);
    }
}
