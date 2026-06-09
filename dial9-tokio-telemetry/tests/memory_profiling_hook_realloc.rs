#![cfg(feature = "memory-profiling")]
#![cfg(feature = "analysis")]
#![cfg(target_os = "linux")]
//! Test that realloc emits both AllocEvent and FreeEvent when liveset is on.

mod common;

use common::{BytesCapturingWriter, decode_all};
use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_tokio_telemetry::telemetry::TracedRuntime;
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
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
    let (writer, batches) = BytesCapturingWriter::new();

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

    assert!(
        ALLOC_COUNT.load(Ordering::Relaxed) > 0,
        "expected CountingAllocator to have been called"
    );

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);
    let allocs: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::AllocEvent(_)))
        .collect();
    let frees: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::FreeEvent(_)))
        .collect();

    assert!(
        !allocs.is_empty(),
        "expected at least one AllocEvent from realloc path"
    );
    assert!(
        !frees.is_empty(),
        "expected at least one FreeEvent when liveset tracking is on"
    );

    // Verify every FreeEvent matches a prior AllocEvent on (addr, size, alloc_timestamp_ns).
    use std::collections::HashMap;
    let mut live: HashMap<u64, (u64, u64)> = HashMap::new(); // addr -> (size, ts)
    let mut matched_frees = 0usize;
    for ev in &events {
        match ev {
            Dial9Event::AllocEvent(a) => {
                live.insert(a.addr, (a.size, a.timestamp_ns));
            }
            Dial9Event::FreeEvent(f) => {
                let Some((recorded_size, recorded_ts)) = live.remove(&f.addr) else {
                    panic!(
                        "FreeEvent at addr {:#x} has no matching prior AllocEvent \
                         (free size={}, alloc_ts={})",
                        f.addr, f.size, f.alloc_timestamp_ns
                    );
                };
                assert_eq!(recorded_size, f.size);
                assert_eq!(recorded_ts, f.alloc_timestamp_ns);
                matched_frees += 1;
            }
            _ => {}
        }
    }
    assert_eq!(matched_frees, frees.len());
}
