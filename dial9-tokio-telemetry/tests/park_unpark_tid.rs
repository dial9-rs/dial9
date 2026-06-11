//! Test that WorkerParkEvent and WorkerUnparkEvent include a non-zero tid field.

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::{InMemoryWriter, TracedRuntime};
use serde::Deserialize;

/// Tagged union over the events this test cares about.
#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum ParkOrUnpark {
    WorkerParkEvent {
        tid: u32,
    },
    WorkerUnparkEvent {
        tid: u32,
    },
    #[serde(other)]
    Other,
}

#[test]
fn worker_park_unpark_events_carry_nonzero_tid() {
    let (capture, batches) = capture_processor();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    // Generate park/unpark cycles by spawning work that yields.
    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..20 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Sleep briefly to ensure workers park.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

    let batches = batches.lock().unwrap();
    let events: Vec<ParkOrUnpark> = decode_all(&batches);

    let park_tids: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            ParkOrUnpark::WorkerParkEvent { tid, .. } => Some(*tid),
            _ => None,
        })
        .collect();

    let unpark_tids: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            ParkOrUnpark::WorkerUnparkEvent { tid, .. } => Some(*tid),
            _ => None,
        })
        .collect();

    assert!(
        !park_tids.is_empty(),
        "expected at least one WorkerPark event"
    );
    assert!(
        !unpark_tids.is_empty(),
        "expected at least one WorkerUnpark event"
    );

    for tid in &park_tids {
        assert_ne!(*tid, 0, "WorkerPark tid must be non-zero");
    }
    for tid in &unpark_tids {
        assert_ne!(*tid, 0, "WorkerUnpark tid must be non-zero");
    }
}
