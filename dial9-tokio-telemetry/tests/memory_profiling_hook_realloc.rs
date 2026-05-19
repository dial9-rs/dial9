#![cfg(feature = "memory-profiling")]
#![cfg(feature = "analysis")]
//! Test that realloc emits both AllocEvent and FreeEvent when liveset is on.

mod common;

use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_tokio_telemetry::telemetry::{TelemetryEvent, TracedRuntime};
use std::alloc::{GlobalAlloc, Layout};
use std::time::Duration;

#[test]
fn hook_realloc_emits_alloc_and_free_when_liveset_on() {
    let (writer, events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .build_and_start_with_writer(builder, writer)
        .unwrap();

    let handle = guard.handle();
    let _mem_guard = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            // Very low sample rate so nearly every alloc is sampled.
            .sample_rate_bytes(64)
            .track_liveset(true)
            .rng_seed(42)
            .build(),
    )
    .install(handle)
    .expect("install should succeed");

    // Exercise realloc via the Dial9Allocator directly.
    let allocator = Dial9Allocator::system();
    runtime.block_on(async {
        for _ in 0..50 {
            let layout = Layout::from_size_align(64, 8).unwrap();
            // SAFETY: layout is valid.
            let ptr = unsafe { allocator.alloc(layout) };
            assert!(!ptr.is_null());
            // SAFETY: ptr was allocated with layout, new_size > 0.
            let new_ptr = unsafe { allocator.realloc(ptr, layout, 256) };
            assert!(!new_ptr.is_null());
            let new_layout = Layout::from_size_align(256, 8).unwrap();
            // SAFETY: new_ptr was returned by realloc.
            unsafe { allocator.dealloc(new_ptr, new_layout) };
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    let events = events.lock().unwrap();
    let allocs: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
        .collect();
    let frees: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
        .collect();

    // With sample_rate=64 and 50 iterations of alloc(64)+realloc(64→256)+dealloc(256),
    // we expect many alloc events and at least some free events (from the realloc
    // decomposition and the final dealloc).
    assert!(
        !allocs.is_empty(),
        "expected at least one AllocEvent from realloc path"
    );
    assert!(
        !frees.is_empty(),
        "expected at least one FreeEvent when liveset tracking is on"
    );
}
