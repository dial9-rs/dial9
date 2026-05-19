#![cfg(feature = "memory-profiling")]
#![cfg(feature = "analysis")]
//! Test that the allocator hook captures sampled allocations into the trace.

mod common;

use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_tokio_telemetry::telemetry::{TelemetryEvent, TracedRuntime};
use std::alloc::{GlobalAlloc, Layout};
use std::time::Duration;

#[test]
fn hook_captures_sampled_allocations() {
    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .build_and_start_with_writer(builder, writer)
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

    // Exercise the hook by calling Dial9Allocator methods directly.
    let allocator = Dial9Allocator::system();
    runtime.block_on(async {
        for _ in 0..100 {
            let layout = Layout::from_size_align(1024, 8).unwrap();
            // SAFETY: layout is valid.
            let ptr = unsafe { allocator.alloc(layout) };
            assert!(!ptr.is_null());
            std::hint::black_box(ptr);
            // SAFETY: ptr was allocated with this layout.
            unsafe { allocator.dealloc(ptr, layout) };
        }
        // Give the flush thread time to drain.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let allocs: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
        .collect();

    assert!(
        !allocs.is_empty(),
        "expected at least one AllocEvent from sampled allocations, got 0"
    );

    // Verify the event has reasonable fields.
    match &allocs[0] {
        TelemetryEvent::Alloc {
            size, callchain, ..
        } => {
            assert!(*size > 0, "size should be non-zero");
            assert!(
                !callchain.is_empty(),
                "callchain should have at least one frame"
            );
        }
        _ => unreachable!(),
    }
}
