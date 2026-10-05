//! `CpuProfiler`'s backend and sample counter, read through the recorder.
//!
//! Its own test binary: ctimer writes to a process-wide sample buffer that the
//! library's unit tests exercise directly, so running there would race them.
#![cfg(all(target_os = "linux", feature = "cpu-profiling"))]

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_core::test_util::drain_encoded_batches;
use dial9_perf_self_profile::{ActiveCpuBackend, CpuProfiler, CpuProfilingConfig, RecorderPerfExt};
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

fn cpu_samples(batches: &[Vec<u8>]) -> u64 {
    let mut n = 0;
    for bytes in batches {
        let mut decoder = dial9_trace_format::decoder::Decoder::new(bytes)
            .expect("encoded batch should have a valid trace header");
        decoder
            .for_each_event(|raw| {
                if let Ok(DecodedEvent::CpuSampleEvent(sample)) = raw.deserialize()
                    && sample.source == 0
                {
                    n += 1;
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
            .with_source(|p: &mut CpuProfiler| (p.effective_backend(), p.samples_seen()))
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
    assert_eq!(seen, cpu_samples(&batches));
    rec.graceful_shutdown(Duration::ZERO);
}
