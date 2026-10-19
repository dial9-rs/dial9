//! The profilers' backends and sample counters, read through the recorder.
//!
//! Its own test binary: ctimer writes to a process-wide sample buffer that the
//! library's unit tests also use, so these tests would race them there.
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

/// A profiler forced onto ctimer reports `Ctimer`, and `samples_drained`
/// equals the CPU samples written to the trace. ctimer needs no perf access,
/// so this runs where perf is blocked.
#[test]
fn ctimer_reports_its_backend_and_counts_drained_samples() {
    // Paused so the flush thread doesn't drain: the test drains, so it sees
    // every sample it counts.
    let rec = recorder(MemoryBuffer::new(16 << 20).expect("writer"))
        .with_cpu_profiling(CpuProfilingConfig::with_ctimer_backend())
        .paused()
        .build();
    let handle = rec.handle().clone();
    let shared = rec.shared().expect("live recorder").clone();
    let backend_and_drained = || {
        handle
            .with_source(|p: &mut CpuProfiler| (p.active_backend().clone(), p.samples_drained()))
            .expect("ctimer profiler starts without perf access")
    };
    let (backend, samples) = backend_and_drained();
    assert!(
        matches!(backend, ActiveCpuBackend::Ctimer { .. }),
        "{backend:?}"
    );
    assert_eq!(samples, 0);

    // ctimer samples only tracked threads. Spin 50ms, then drain; repeat, since
    // a busy host may starve this thread for a round.
    let tracking = handle.track_current_thread().expect("track this thread");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut batches = Vec::new();
    while backend_and_drained().1 == 0 && Instant::now() < deadline {
        let start = Instant::now();
        let mut x = 0u64;
        while start.elapsed() < Duration::from_millis(50) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        shared.flush_sources();
        batches.extend(drain_encoded_batches(&shared));
    }
    drop(tracking);

    let (_, drained) = backend_and_drained();
    assert!(drained > 0, "no CPU samples within 5s of spinning");
    assert_eq!(drained, count_samples(&batches, CPU_PROFILE));
    rec.graceful_shutdown(Duration::ZERO);
}

/// The scheduler profiler's `samples_drained` counts exactly the scheduler
/// samples a drain writes. Skips where perf is blocked, like the crate's other
/// perf tests.
#[test]
fn sched_profiler_counts_drained_samples() {
    // Paused so the flush thread doesn't drain: the test drains, so it sees
    // every sample it counts.
    let rec = recorder(MemoryBuffer::new(16 << 20).expect("writer"))
        .with_sched_events(SchedEventConfig::default())
        .paused()
        .build();
    let handle = rec.handle().clone();
    let shared = rec.shared().expect("live recorder").clone();
    let samples_drained = || handle.with_source(|p: &mut SchedProfiler| p.samples_drained());
    if samples_drained().is_none() {
        eprintln!("skipping: the scheduler profiler didn't start (perf blocked?)");
        rec.graceful_shutdown(Duration::ZERO);
        return;
    }
    assert_eq!(samples_drained(), Some(0));

    // The scheduler profiler samples context switches. Joining a just-spawned
    // thread blocks this tracked thread, which switches it out.
    let tracking = handle.track_current_thread().expect("track this thread");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut batches = Vec::new();
    while samples_drained() == Some(0) && Instant::now() < deadline {
        for _ in 0..20 {
            std::thread::spawn(|| {}).join().expect("empty thread");
        }
        shared.flush_sources();
        batches.extend(drain_encoded_batches(&shared));
    }
    drop(tracking);

    let drained = samples_drained().expect("still registered");
    assert!(drained > 0, "no scheduler samples within 5s of blocking");
    assert_eq!(drained, count_samples(&batches, SCHED_EVENT));
    rec.graceful_shutdown(Duration::ZERO);
}
