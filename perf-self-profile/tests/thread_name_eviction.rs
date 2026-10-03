//! A tracked thread's cached name is dropped once it stops, so a later thread
//! with the same tid reads its own name. A thread that renames itself between
//! two tracked stretches stands in for tid reuse, which can't be forced.

#![cfg(all(target_os = "linux", feature = "cpu-profiling"))]

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_core::test_util::drain_encoded_batches;
use dial9_core::thread::current_tid;
use dial9_perf_self_profile::{CpuProfilingConfig, RecorderPerfExt};
use serde::Deserialize;
use std::sync::{Arc, Barrier};
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
    tid: u32,
    thread_name: Option<String>,
}

/// Names on `tid`'s CPU samples in `batches`.
fn sample_names(batches: &[Vec<u8>], tid: u32) -> Vec<Option<String>> {
    let mut names = Vec::new();
    for bytes in batches {
        let mut decoder = dial9_trace_format::decoder::Decoder::new(bytes)
            .expect("encoded batch should have a valid trace header");
        decoder
            .for_each_event(|raw| {
                if let Ok(DecodedEvent::CpuSampleEvent(sample)) = raw.deserialize()
                    && sample.tid == tid
                {
                    names.push(sample.thread_name);
                }
            })
            .expect("encoded batch should decode");
    }
    names
}

fn burn(cpu: Duration) {
    let start = Instant::now();
    let mut x = 0u64;
    while start.elapsed() < cpu {
        x = std::hint::black_box(x.wrapping_add(1));
    }
}

fn rename_current_thread(name: &std::ffi::CStr) {
    // SAFETY: PR_SET_NAME reads a NUL-terminated string of at most 16 bytes.
    let rc = unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr()) };
    assert_eq!(
        rc,
        0,
        "prctl(PR_SET_NAME): {}",
        std::io::Error::last_os_error()
    );
}

#[test]
fn stopped_thread_name_is_not_reused() {
    // Paused so the flush thread doesn't drain: the test drives every drain.
    let rec = recorder(MemoryBuffer::new(16 << 20).expect("writer"))
        .with_cpu_profiling(CpuProfilingConfig::default())
        .paused()
        .build();
    let handle = rec.handle().clone();
    let shared = rec.shared().expect("live recorder").clone();
    let step = Arc::new(Barrier::new(2));

    let thread = std::thread::Builder::new()
        .name("first".into())
        .spawn({
            let step = Arc::clone(&step);
            move || {
                let tid = current_tid();
                let tracking = handle.track_current_thread().expect("track thread");
                burn(Duration::from_millis(200));
                step.wait(); // 1: sampled as "first"
                step.wait(); // 2: drained
                drop(tracking);
                step.wait(); // 3: stopped
                step.wait(); // 4: drained again
                rename_current_thread(c"second");
                let _tracking = handle.track_current_thread().expect("track thread again");
                burn(Duration::from_millis(200));
                step.wait(); // 5: sampled as "second"
                step.wait(); // 6: drained
                tid
            }
        })
        .expect("spawn thread");

    step.wait(); // 1
    shared.flush_sources();
    let before = drain_encoded_batches(&shared);
    step.wait(); // 2
    step.wait(); // 3
    shared.flush_sources(); // the drain after the stop evicts "first"
    drain_encoded_batches(&shared);
    step.wait(); // 4
    step.wait(); // 5
    shared.flush_sources();
    let after = drain_encoded_batches(&shared);
    step.wait(); // 6
    let tid = thread.join().expect("thread");
    rec.graceful_shutdown(Duration::ZERO);

    let before = sample_names(&before, tid);
    let after = sample_names(&after, tid);
    assert!(!before.is_empty(), "no samples before the stop");
    assert!(!after.is_empty(), "no samples after the rename");
    assert!(
        before.iter().all(|n| n.as_deref() == Some("first")),
        "{before:?}"
    );
    assert!(
        after.iter().all(|n| n.as_deref() == Some("second")),
        "the stopped thread's name was kept: {after:?}"
    );
}
