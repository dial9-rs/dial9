#![cfg(feature = "memory-profiling")]
#![cfg(feature = "analysis")]
#![cfg(target_os = "linux")]
//! Test that realloc emits both AllocEvent and FreeEvent when liveset is on.

mod common;

use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_tokio_telemetry::telemetry::{TelemetryEvent, TracedRuntime};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

impl CountingAllocator {
    const fn new() -> Self {
        Self
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Dial9Allocator<CountingAllocator> = Dial9Allocator::new(CountingAllocator::new());

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
            .sample_rate_bytes(64)
            .track_liveset(true)
            .rng_seed(42)
            .build(),
    )
    .install(handle)
    .expect("install should succeed");

    runtime.block_on(async {
        // Push in a loop to force Vec reallocation (capacity growth).
        let mut v: Vec<u8> = Vec::new();
        for i in 0..1000u16 {
            v.push((i & 0xff) as u8);
        }
        std::hint::black_box(&v);
        drop(v);

        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(runtime);
    drop(guard);

    // Verify the inner allocator was actually called.
    assert!(
        ALLOC_COUNT.load(Ordering::Relaxed) > 0,
        "expected CountingAllocator to have been called"
    );

    let events = events.lock().unwrap();
    let allocs: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::Alloc { .. }))
        .collect();
    let frees: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, TelemetryEvent::Free { .. }))
        .collect();

    assert!(
        !allocs.is_empty(),
        "expected at least one AllocEvent from realloc path"
    );
    assert!(
        !frees.is_empty(),
        "expected at least one FreeEvent when liveset tracking is on"
    );
}
