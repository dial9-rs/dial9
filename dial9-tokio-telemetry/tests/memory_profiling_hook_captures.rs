#![cfg(feature = "memory-profiling")]
#![cfg(feature = "analysis")]
#![cfg(target_os = "linux")]
//! Test that the allocator hook captures sampled allocations into the trace.

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{InMemoryWriter, TracedRuntime};
use std::time::Duration;

#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

#[test]
fn hook_captures_sampled_allocations() {
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
            .sample_rate_bytes(1024)
            .rng_seed(42)
            .build(),
    )
    .install(handle)
    .expect("install should succeed");

    runtime.block_on(async {
        for _ in 0..100 {
            let v: Vec<u8> = Vec::with_capacity(1024);
            std::hint::black_box(v);
        }
        // Give the flush thread time to drain.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);
    let allocs: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::AllocEvent(_)))
        .collect();

    assert!(
        !allocs.is_empty(),
        "expected at least one AllocEvent from sampled allocations, got 0"
    );

    // Verify the event has reasonable fields.
    if let Dial9Event::AllocEvent(e) = &allocs[0] {
        assert!(e.size > 0, "size should be non-zero");
        assert!(
            !e.callchain.is_empty(),
            "callchain should have at least one frame"
        );
    }
}
