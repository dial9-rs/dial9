//! Sched event capture on plain `std::thread`s, no Tokio anywhere.
//!
//! Tokio workers enroll with the per-thread sources on their first poll. Every
//! other thread has to ask, via `Dial9Handle::track_current_thread`.

#![cfg(all(target_os = "linux", feature = "cpu-profiling"))]

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_core::test_util::drain_encoded_batches;
use dial9_core::thread::current_tid;
use dial9_perf_self_profile::{SchedEventConfig, SchedProfiler};
use serde::Deserialize;
use std::sync::{Arc, Barrier};
use std::time::Duration;

const THREADS: usize = 2;

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
    /// `CpuSampleSource` on the wire: 0 = CPU profile, 1 = sched event.
    source: u8,
}

/// Count this process's open perf_event fds.
fn count_perf_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("read /proc/self/fd")
        .filter_map(|e| e.ok())
        .filter(|e| {
            std::fs::read_link(e.path())
                .map(|p| p.to_string_lossy().contains("perf_event"))
                .unwrap_or(false)
        })
        .count()
}

/// Tids that produced at least one sched-event sample.
fn sched_event_tids(batches: &[Vec<u8>]) -> Vec<u32> {
    let mut tids = Vec::new();
    for bytes in batches {
        let mut decoder = dial9_trace_format::decoder::Decoder::new(bytes)
            .expect("encoded batch should have a valid trace header");
        decoder
            .for_each_event(|raw| {
                if let Ok(DecodedEvent::CpuSampleEvent(sample)) = raw.deserialize()
                    && sample.source == 1
                {
                    tids.push(sample.tid);
                }
            })
            .expect("encoded batch should decode");
    }
    tids
}

#[test]
fn plain_threads_capture_sched_events_once_they_opt_in() {
    let sched = SchedProfiler::new(SchedEventConfig::default()).expect("sched profiler");

    // Paused so the flush thread does not race us for the collector; this test
    // drives `flush_sources` and drains by hand.
    let rec = recorder(MemoryBuffer::new(1 << 20).expect("writer"))
        .source(sched)
        .paused()
        .build();
    let handle = rec.handle().clone();
    let shared = rec.shared().expect("enabled recorder").clone();

    let baseline_fds = count_perf_fds();

    let sampled = Arc::new(Barrier::new(THREADS + 1));
    let drained = Arc::new(Barrier::new(THREADS + 1));

    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let handle = handle.clone();
            let sampled = Arc::clone(&sampled);
            let drained = Arc::clone(&drained);
            std::thread::spawn(move || {
                let _tracking = handle.track_current_thread().expect("track thread");
                let tid = current_tid();
                // Each sleep deschedules the thread, which is the event we sample.
                for _ in 0..10 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                sampled.wait();
                // Hold the guard until the main thread has drained: dropping it
                // closes the perf fd and unmaps the ring, losing unread samples.
                drained.wait();
                tid
            })
        })
        .collect();

    sampled.wait();
    assert_eq!(
        count_perf_fds(),
        baseline_fds + THREADS,
        "expected one perf fd per tracked thread"
    );

    shared.flush_sources();
    let batches = drain_encoded_batches(&shared);
    drained.wait();

    let tids: Vec<u32> = workers
        .into_iter()
        .map(|w| w.join().expect("worker thread"))
        .collect();

    assert_eq!(
        count_perf_fds(),
        baseline_fds,
        "dropping the guards should close their perf fds"
    );

    let sampled_tids = sched_event_tids(&batches);
    for tid in &tids {
        assert!(
            sampled_tids.contains(tid),
            "no sched events for thread {tid}, got {sampled_tids:?}"
        );
    }

    rec.graceful_shutdown(Duration::ZERO);
}
