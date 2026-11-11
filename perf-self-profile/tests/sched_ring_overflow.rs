//! A sched ring too small for the context switches between two flushes drops
//! samples, and the profiler reports how many.

#![cfg(all(target_os = "linux", feature = "cpu-profiling"))]

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_core::source::Source;
use dial9_core::test_util::drain_encoded_batches;
use dial9_perf_self_profile::{SchedEventConfig, SchedProfiler};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Barrier};
use std::time::Duration;

/// Deschedule the calling thread often enough to overflow a one-page ring,
/// 64 KiB pages included: a context-switch record with its callchain is over
/// 100 bytes. Each rendezvous blocks until the peer thread answers.
fn context_switch_burst() {
    let (ping, pings) = sync_channel::<()>(0);
    let (pong, pongs) = sync_channel::<()>(0);
    let peer = std::thread::spawn(move || {
        while pings.recv().is_ok() {
            pong.send(()).expect("burst thread alive");
        }
    });
    for _ in 0..1000 {
        ping.send(()).expect("peer alive");
        pongs.recv().expect("peer alive");
    }
    drop(ping);
    peer.join().expect("peer thread");
}

#[test]
fn overflowing_ring_reports_lost_samples() {
    // Rounded up to a single page.
    let sched = SchedProfiler::new(SchedEventConfig::default().ring_buffer_size(1))
        .expect("sched profiler");

    // Paused so the flush thread does not drain the ring; this test drives
    // `flush_sources` by hand.
    let rec = recorder(MemoryBuffer::new(1 << 20).expect("writer"))
        .source(sched)
        .paused()
        .build();
    let handle = rec.handle().clone();
    let shared = rec.shared().expect("enabled recorder").clone();

    let step = Arc::new(Barrier::new(2));
    let worker = {
        let handle = handle.clone();
        let step = Arc::clone(&step);
        std::thread::spawn(move || {
            let _tracking = handle.track_current_thread().expect("track thread");
            context_switch_burst();
            step.wait(); // ring is full
            step.wait(); // main drained it
            // The kernel writes the lost count with the next record that fits.
            context_switch_burst();
            step.wait();
            // Hold the guard until the main thread has drained again.
            step.wait();
        })
    };

    let metadata = || {
        handle
            .with_source::<SchedProfiler, _>(|sched| {
                let mut out = Vec::new();
                sched.segment_metadata(&mut out);
                out
            })
            .expect("sched source registered")
    };
    let lost_samples = |entries: &[(String, String)]| {
        entries
            .iter()
            .find(|(key, _)| key == "sched.profile.lost_samples")
            .map(|(_, value)| value.parse::<u64>().expect("lost count is a number"))
    };

    step.wait();
    shared.flush_sources();
    step.wait();
    step.wait();
    shared.flush_sources();
    drain_encoded_batches(&shared);

    let lost = lost_samples(&metadata());
    assert!(
        lost.is_some_and(|lost| lost > 0),
        "expected lost samples from a one-page ring, got {lost:?}"
    );
    step.wait();
    worker.join().expect("worker thread");
    rec.graceful_shutdown(Duration::ZERO);
}
