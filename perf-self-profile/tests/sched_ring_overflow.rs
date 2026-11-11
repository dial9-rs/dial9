//! A sched ring too small for the context switches between two flushes drops
//! samples, and the profiler reports how many.

#![cfg(all(target_os = "linux", feature = "cpu-profiling"))]

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_core::source::Source;
use dial9_core::test_util::drain_encoded_batches;
use dial9_perf_self_profile::{SchedEventConfig, SchedProfiler};
use std::sync::mpsc::sync_channel;
use std::time::Duration;

/// Deschedule the calling thread often enough to overflow a one-page ring,
/// 64 KiB pages included: a context-switch record with its callchain is over
/// 100 bytes. A rendezvous blocks until the peer thread answers, unless the
/// peer is quick enough to answer while the channel still spins, hence the
/// generous count.
fn context_switch_burst() {
    let (ping, pings) = sync_channel::<()>(0);
    let (pong, pongs) = sync_channel::<()>(0);
    let peer = std::thread::spawn(move || {
        while pings.recv().is_ok() {
            pong.send(()).expect("burst thread alive");
        }
    });
    for _ in 0..20_000 {
        ping.send(()).expect("peer alive");
        pongs.recv().expect("peer alive");
    }
    drop(ping);
    peer.join().expect("peer thread");
}

#[test]
fn overflowing_ring_reports_lost_samples() {
    // Rounded up to a single page.
    let sched = match SchedProfiler::new(SchedEventConfig::default().ring_buffer_size(1)) {
        Ok(sched) => sched,
        // perf_event_open blocked (e.g. restricted CI); nothing to verify.
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => return,
        Err(e) => panic!("failed to create sched profiler: {e}"),
    };

    // Paused so the flush thread does not drain the ring; this test drives
    // `flush_sources` by hand.
    let rec = recorder(MemoryBuffer::new(1 << 20).expect("writer"))
        .source(sched)
        .paused()
        .build();
    let handle = rec.handle().clone();
    let shared = rec.shared().expect("enabled recorder").clone();

    let tracking = handle.track_current_thread().expect("track thread");
    context_switch_burst();
    shared.flush_sources();
    // The kernel writes the lost count with the next record that fits.
    context_switch_burst();
    shared.flush_sources();
    drain_encoded_batches(&shared);
    drop(tracking);

    let (metadata, samples_lost) = handle
        .with_source::<SchedProfiler, _>(|sched| {
            let mut out = Vec::new();
            sched.segment_metadata(&mut out);
            (out, sched.samples_lost())
        })
        .expect("sched source registered");
    let reported = metadata
        .iter()
        .find(|(key, _)| key == "sched.profile.lost_samples")
        .map(|(_, value)| value.parse::<u64>().expect("lost count is a number"));

    assert!(
        samples_lost > 0,
        "expected lost samples from a one-page ring"
    );
    assert_eq!(reported, Some(samples_lost));

    rec.graceful_shutdown(Duration::ZERO);
}
