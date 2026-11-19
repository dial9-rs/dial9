//! A sched ring too small for the context switches between two flushes drops
//! samples, and the profiler reports how many in the trace.

#![cfg(all(target_os = "linux", feature = "cpu-profiling"))]

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_core::test_util::drain_encoded_batches;
use dial9_perf_self_profile::{SchedEventConfig, SchedProfiler};
use serde::Deserialize;
use std::num::NonZeroUsize;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum DecodedEvent {
    CpuSamplesLostEvent(DecodedLost),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct DecodedLost {
    /// `CpuSampleSource` on the wire: 1 = sched event.
    source: u8,
    lost: u64,
    total_lost: u64,
}

fn lost_events(batches: &[Vec<u8>]) -> Vec<DecodedLost> {
    let mut events = Vec::new();
    for bytes in batches {
        let mut decoder = dial9_trace_format::decoder::Decoder::new(bytes)
            .expect("encoded batch should have a valid trace header");
        decoder
            .for_each_event(|raw| {
                // Other event types decode to `Other`, so an error here is a
                // decode failure.
                if let DecodedEvent::CpuSamplesLostEvent(lost) =
                    raw.deserialize().expect("event should deserialize")
                {
                    events.push(lost);
                }
            })
            .expect("encoded batch should decode");
    }
    events
}

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
    let sched =
        match SchedProfiler::new(SchedEventConfig::default().ring_buffer_size(NonZeroUsize::MIN)) {
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
    let events = lost_events(&drain_encoded_batches(&shared));
    drop(tracking);

    let samples_lost = handle
        .with_source::<SchedProfiler, _>(|sched| sched.samples_lost())
        .expect("sched source registered");

    assert!(
        samples_lost > 0,
        "expected lost samples from a one-page ring"
    );
    assert!(
        events
            .iter()
            .all(|event| event.source == 1 && event.lost > 0)
    );
    assert_eq!(
        events.iter().map(|event| event.lost).sum::<u64>(),
        samples_lost
    );
    assert_eq!(
        events.last().map(|event| event.total_lost),
        Some(samples_lost)
    );

    rec.graceful_shutdown(Duration::ZERO);
}
