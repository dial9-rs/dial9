#![cfg(all(feature = "memory-profiling", target_os = "linux"))]
//! Collector initialization on a thread's first free must not look like a leak.

use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_core::test_util::drain_encoded_batches;
use dial9_core::thread::current_tid;
use dial9_perf_self_profile::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_trace_format::decoder::Decoder;
use serde_json::Value;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static PROFILE_THREAD: Cell<bool> = const { Cell::new(false) };
}

struct WorkerAllocator;

#[global_allocator]
static ALLOC: WorkerAllocator = WorkerAllocator;

// Keep setup and decoding out of the profiler, so the worker initializes the
// first collector arena and no other thread's samples can obscure the result.
unsafe impl GlobalAlloc for WorkerAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if PROFILE_THREAD.with(Cell::get) {
            unsafe { Dial9Allocator::system().alloc(layout) }
        } else {
            unsafe { System.alloc(layout) }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if PROFILE_THREAD.with(Cell::get) {
            unsafe { Dial9Allocator::system().dealloc(ptr, layout) };
        } else {
            unsafe { System.dealloc(ptr, layout) };
        }
    }
}

#[test]
fn first_dealloc_suppresses_collector_allocations_and_preserves_sampling() {
    let rec = recorder(MemoryBuffer::new(1 << 20).expect("memory buffer"))
        .paused()
        .build();
    let old_allocation = vec![0u8; 512];
    let _guard = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(1)
            .track_liveset(true)
            .build(),
    )
    .install(rec.handle().clone())
    .expect("memory profiler install");

    // Setup uses System until this flag is set, so the worker's first hook is
    // a free of previously untracked memory, with no collector or sampling TLS.
    let worker = std::thread::spawn(move || {
        let tid = current_tid();
        PROFILE_THREAD.with(|enabled| enabled.set(true));
        std::hint::black_box(&old_allocation);
        drop(old_allocation);
        let allocation = vec![0u8; 1536];
        std::hint::black_box(&allocation);
        drop(allocation);
        tid
    });
    let tid = worker
        .join()
        .expect("worker thread, including TLS teardown");

    let shared = rec.shared().expect("enabled recorder");
    shared.flush_sources();
    let mut events = Vec::new();
    for bytes in drain_encoded_batches(shared) {
        Decoder::new(&bytes)
            .expect("trace header")
            .for_each_event(|raw| {
                let event: Value = raw.deserialize().expect("decode event");
                if event["tid"] == tid {
                    events.push(event);
                }
            })
            .expect("decode trace");
    }
    let allocs: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "AllocEvent")
        .collect();
    assert_eq!(allocs.len(), 1, "collector storage must not be sampled");
    let alloc = allocs[0];
    assert_eq!(alloc["size"], 1536);
    assert!(events.iter().any(|free| {
        free["event"] == "FreeEvent"
            && free["addr"] == alloc["addr"]
            && free["size"] == alloc["size"]
            && free["alloc_timestamp_ns"] == alloc["timestamp_ns"]
    }));
}
