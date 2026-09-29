//! `CpuProfiler`'s backend and sample counter through a live recorder.
//!
//! Its own test binary: ctimer writes to a process-wide sample buffer that the
//! library's unit tests exercise directly, so running there would race them.
#![cfg(all(target_os = "linux", feature = "cpu-profiling"))]

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_perf_self_profile::{ActiveCpuBackend, CpuProfiler, CpuProfilingConfig, RecorderPerfExt};
use std::time::{Duration, Instant};

/// A forced ctimer backend reports itself, and the counter advances once the
/// flush thread drains samples from a tracked thread. ctimer needs no perf
/// access.
#[test]
fn ctimer_reports_its_backend_and_counts_samples() {
    let rec = recorder(MemoryBuffer::new(16 << 20).expect("writer"))
        .with_cpu_profiling(CpuProfilingConfig::with_ctimer_backend())
        .build();
    let handle = rec.handle().clone();
    let read = || {
        handle
            .with_source(|p: &mut CpuProfiler| (p.effective_backend(), p.samples_seen()))
            .expect("ctimer needs no perf access, so the profiler starts")
    };
    let (backend, before) = read();
    assert_eq!(backend, ActiveCpuBackend::Ctimer);

    // ctimer samples only tracked threads.
    let _tracked = handle.track_current_thread().expect("track this thread");
    let burn = Instant::now();
    let mut x = 0u64;
    while burn.elapsed() < Duration::from_millis(300) {
        x = std::hint::black_box(x.wrapping_add(1));
    }
    // The flush thread drains about every 5ms.
    let deadline = Instant::now() + Duration::from_secs(5);
    let after = loop {
        let (_, now) = read();
        if now > before || Instant::now() > deadline {
            break now;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(after > before, "300ms of CPU at 99Hz produced no samples");
    rec.graceful_shutdown(Duration::from_secs(5));
}
