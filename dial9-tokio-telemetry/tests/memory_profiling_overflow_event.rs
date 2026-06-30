#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Test that MemoryProfileOverflowEvent is emitted when ring buffers overflow.

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_tokio_telemetry::telemetry::{InMemoryWriter, TracedRuntime};
use serde::Deserialize;
use std::time::Duration;

#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum OverflowEvent {
    MemoryProfileOverflowEvent {
        #[allow(dead_code)]
        timestamp_ns: u64,
        dropped_allocs: u64,
        dropped_frees: u64,
    },
    #[serde(other)]
    Other,
}

#[test]
fn overflow_event_emitted_when_ring_overflows() {
    let (capture, batches) = capture_processor();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    let handle = guard.handle();
    // Use a tiny ring (capacity 4) so it overflows easily under allocation pressure.
    let _mem_guard = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(1) // sample every allocation
            .ring_capacity(4)
            .rng_seed(42)
            .build(),
    )
    .install(handle)
    .expect("install should succeed");

    // Generate enough allocations to overflow the tiny ring.
    runtime.block_on(async {
        for _ in 0..1000 {
            let v: Vec<u8> = vec![0u8; 64];
            std::hint::black_box(&v);
            drop(v);
        }
        // Wait for at least one flush cycle to pick up the overflow.
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

    let batches = batches.lock().unwrap();
    let events: Vec<OverflowEvent> = decode_all(&batches);

    let overflows: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            OverflowEvent::MemoryProfileOverflowEvent {
                dropped_allocs,
                dropped_frees,
                ..
            } => Some((*dropped_allocs, *dropped_frees)),
            _ => None,
        })
        .collect();

    assert!(
        !overflows.is_empty(),
        "expected at least one MemoryProfileOverflowEvent"
    );

    let total_dropped_allocs: u64 = overflows.iter().map(|(a, _)| a).sum();
    let total_dropped_frees: u64 = overflows.iter().map(|(_, f)| f).sum();

    // With ring capacity 4 and 1000 allocations at sample_rate=1, we should
    // have many dropped samples.
    assert!(
        total_dropped_allocs > 0 || total_dropped_frees > 0,
        "expected non-zero drops, got allocs={total_dropped_allocs} frees={total_dropped_frees}"
    );
}
