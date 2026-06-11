//! End-to-end test: encode events via TracedRuntime, decode via serde into
//! the built-in analysis event structs.

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{InMemoryWriter, TracedRuntime};

#[test]
fn decode_builtin_events_via_serde() {
    let (capture, batches) = capture_processor();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..10 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Let workers park
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

    let batches = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&batches);

    // We should have at least some poll start/end events
    let poll_starts: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::PollStartEvent(_)))
        .collect();
    assert!(
        !poll_starts.is_empty(),
        "expected at least one PollStartEvent"
    );

    let poll_ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::PollEndEvent(_)))
        .collect();
    assert!(!poll_ends.is_empty(), "expected at least one PollEndEvent");

    // Should have park/unpark events
    let parks: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::WorkerParkEvent(_)))
        .collect();
    assert!(!parks.is_empty(), "expected at least one WorkerParkEvent");

    let unparks: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::WorkerUnparkEvent(_)))
        .collect();
    assert!(
        !unparks.is_empty(),
        "expected at least one WorkerUnparkEvent"
    );

    // Should have task spawn events
    let spawns: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::TaskSpawnEvent(_)))
        .collect();
    assert!(!spawns.is_empty(), "expected at least one TaskSpawnEvent");

    // Verify a PollStartEvent has sensible data
    if let Dial9Event::PollStartEvent(ps) = &poll_starts[0] {
        assert!(ps.timestamp_ns > 0, "timestamp should be non-zero");
    }

    // Verify a WorkerParkEvent has non-zero tid
    if let Dial9Event::WorkerParkEvent(wp) = &parks[0] {
        assert_ne!(wp.tid, 0, "WorkerPark tid must be non-zero");
    }
}
