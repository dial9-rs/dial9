//! The profilers' backends and sample counters, read through the recorder.
//!
//! Its own test binary: ctimer writes to a process-wide sample buffer that the
//! library's unit tests exercise directly, so running there would race them.
#![cfg(all(target_os = "linux", feature = "cpu-profiling"))]

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_core::test_util::drain_encoded_batches;
use dial9_perf_self_profile::{
    ActiveCpuBackend, CpuProfiler, CpuProfilingConfig, RecorderPerfExt, SchedEventConfig,
    SchedProfiler,
};
use serde::Deserialize;
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum DecodedEvent {
    CpuSampleEvent(DecodedCpuSample),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct DecodedCpuSample {
    /// `CpuSampleSource` on the wire: 0 = CPU profile, 1 = sched event.
    source: u8,
}

/// `CpuSampleSource` values on the wire.
const CPU_PROFILE: u8 = 0;
const SCHED_EVENT: u8 = 1;

fn count_samples(batches: &[Vec<u8>], source: u8) -> u64 {
    let mut n = 0;
    for bytes in batches {
        let mut decoder = dial9_trace_format::decoder::Decoder::new(bytes)
            .expect("encoded batch should have a valid trace header");
        decoder
            .for_each_event(|raw| {
                // Other event types decode to `Other`, so an error here is a
                // decode failure.
                match raw.deserialize().expect("event should deserialize") {
                    DecodedEvent::CpuSampleEvent(sample) if sample.source == source => n += 1,
                    _ => {}
                }
            })
            .expect("encoded batch should decode");
    }
    n
}

/// A forced ctimer backend reports itself, and `samples_seen` counts exactly
/// the CPU samples a drain writes. ctimer needs no perf access.
#[test]
fn ctimer_reports_its_backend_and_counts_drained_samples() {
    // Paused so the flush thread doesn't drain: the test drives the drain.
    let rec = recorder(MemoryBuffer::new(16 << 20).expect("writer"))
        .with_cpu_profiling(CpuProfilingConfig::with_ctimer_backend())
        .paused()
        .build();
    let handle = rec.handle().clone();
    let shared = rec.shared().expect("live recorder").clone();
    let read = || {
        handle
            .with_source(|p: &mut CpuProfiler| (p.active_backend(), p.samples_seen()))
            .expect("ctimer needs no perf access, so the profiler starts")
    };
    let (backend, samples) = read();
    assert!(
        matches!(backend, ActiveCpuBackend::Ctimer { .. }),
        "{backend:?}"
    );
    assert_eq!(samples, 0);

    // ctimer samples only tracked threads. Burn and drain in rounds: a thread
    // starved of CPU on a busy host may need more than one.
    let tracking = handle.track_current_thread().expect("track this thread");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut batches = Vec::new();
    while read().1 == 0 && Instant::now() < deadline {
        let start = Instant::now();
        let mut x = 0u64;
        while start.elapsed() < Duration::from_millis(50) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        shared.flush_sources();
        batches.extend(drain_encoded_batches(&shared));
    }
    drop(tracking);

    let (_, seen) = read();
    assert!(seen > 0, "no CPU samples within 5s of burning");
    assert_eq!(seen, count_samples(&batches, CPU_PROFILE));
    rec.graceful_shutdown(Duration::ZERO);
}

/// The scheduler profiler's `samples_seen` counts exactly the scheduler
/// samples a drain writes. Skips where perf is blocked, like the crate's other
/// perf tests.
#[test]
fn sched_profiler_counts_drained_samples() {
    // Paused so the flush thread doesn't drain: the test drives the drain.
    let rec = recorder(MemoryBuffer::new(16 << 20).expect("writer"))
        .with_sched_events(SchedEventConfig::default())
        .paused()
        .build();
    let handle = rec.handle().clone();
    let shared = rec.shared().expect("live recorder").clone();
    let read = || handle.with_source(|p: &mut SchedProfiler| p.samples_seen());
    if read().is_none() {
        eprintln!("skipping: the scheduler profiler didn't start (perf blocked?)");
        rec.graceful_shutdown(Duration::ZERO);
        return;
    }
    assert_eq!(read(), Some(0));

    // Sleeping switches this tracked thread out, which is what it samples.
    let tracking = handle.track_current_thread().expect("track this thread");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut batches = Vec::new();
    while read() == Some(0) && Instant::now() < deadline {
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(1));
        }
        shared.flush_sources();
        batches.extend(drain_encoded_batches(&shared));
    }
    drop(tracking);

    let seen = read().expect("still registered");
    assert!(seen > 0, "no scheduler samples within 5s of sleeping");
    assert_eq!(seen, count_samples(&batches, SCHED_EVENT));
    rec.graceful_shutdown(Duration::ZERO);
}
