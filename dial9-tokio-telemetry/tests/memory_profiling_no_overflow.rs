#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Test that no MemoryProfileOverflowEvent is emitted when ring has sufficient capacity.

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
        #[allow(dead_code)]
        dropped_allocs: u64,
        #[allow(dead_code)]
        dropped_frees: u64,
    },
    #[serde(other)]
    Other,
}

#[test]
fn no_overflow_event_when_ring_has_capacity() {
    let (capture, batches) = capture_processor();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    let handle = guard.handle();
    let _mem_guard = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(512 * 1024)
            .ring_capacity(4096)
            .rng_seed(42)
            .build(),
    )
    .install(handle)
    .expect("install should succeed");

    runtime.block_on(async {
        for _ in 0..10 {
            let v: Vec<u8> = vec![0u8; 64];
            std::hint::black_box(&v);
            drop(v);
        }
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
        .filter(|e| matches!(e, OverflowEvent::MemoryProfileOverflowEvent { .. }))
        .collect();

    assert!(
        overflows.is_empty(),
        "expected no MemoryProfileOverflowEvent when ring has capacity, got {}",
        overflows.len()
    );
}
