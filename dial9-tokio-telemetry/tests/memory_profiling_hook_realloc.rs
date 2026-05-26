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

    // Stronger property: every FreeEvent must match a previously-recorded
    // AllocEvent on (addr, size, alloc_timestamp_ns). Walk the trace in
    // recording order, maintaining a "live" map keyed by addr; an Alloc
    // overwrites any prior entry at that addr (an address can be reused
    // after a free), and a Free must find the current entry and have its
    // (size, alloc_timestamp_ns) match the recorded Alloc.
    use std::collections::HashMap;
    let mut live: HashMap<u64, (u64, u64)> = HashMap::new(); // addr -> (size, ts)
    let mut matched_frees = 0usize;
    for ev in events.iter() {
        match ev {
            TelemetryEvent::Alloc {
                addr,
                size,
                timestamp_nanos,
                ..
            } => {
                live.insert(*addr, (*size, *timestamp_nanos));
            }
            TelemetryEvent::Free {
                addr,
                size,
                alloc_timestamp_nanos,
                ..
            } => {
                let Some((recorded_size, recorded_ts)) = live.remove(addr) else {
                    panic!(
                        "FreeEvent at addr {addr:#x} has no matching prior AllocEvent \
                         (free size={size}, alloc_ts={alloc_timestamp_nanos})"
                    );
                };
                assert_eq!(
                    recorded_size, *size,
                    "FreeEvent at addr {addr:#x}: size {size} does not match \
                     recorded Alloc size {recorded_size}"
                );
                assert_eq!(
                    recorded_ts, *alloc_timestamp_nanos,
                    "FreeEvent at addr {addr:#x}: alloc_timestamp_ns \
                     {alloc_timestamp_nanos} does not match recorded \
                     Alloc timestamp {recorded_ts}"
                );
                matched_frees = matched_frees.saturating_add(1);
            }
            _ => {}
        }
    }
    assert_eq!(
        matched_frees,
        frees.len(),
        "all {} frees should have matched a prior alloc, only {matched_frees} did",
        frees.len()
    );
}
