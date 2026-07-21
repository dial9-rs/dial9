//! Minimal dial9 + memory profiling example.
//!
//! Sets `Dial9Allocator` as the global allocator, enables memory profiling
//! on the running dial9 recorder, does some allocating work, and writes a trace
//! with `AllocEvent`s to disk.
//!
//!   cargo run -p memory-local

use dial9::Dial9Handle;
use dial9::Dial9TokioHandle;
use dial9::memory::{Dial9Allocator, MemoryProfiler, MemoryProfilingConfig};
use dial9::{DiskBuffer, TracedRuntimeBuilder};
use std::time::Duration;

const TRACE_DIR: &str = "/tmp/memory-local-traces";

// Route every allocation through the sampling allocator.
#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

async fn allocate_some() {
    let mut buffers: Vec<Vec<u8>> = Vec::new();
    for i in 0..64 {
        buffers.push(vec![0u8; 1024 + i * 128]);
    }
    std::hint::black_box(&buffers);
}

fn my_config() -> TracedRuntimeBuilder {
    let writer = DiskBuffer::builder()
        .base_path(TRACE_DIR)
        .max_file_size(10_000_000)
        .max_total_size(50_000_000)
        .build();
    dial9::recorder_or_disabled(writer, |t| {
        t.worker_threads(2);
    })
    .with_task_tracking(true)
}

#[dial9::main(config = my_config)]
async fn main() {
    let _guard = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(16 * 1024) // sample often so the demo has events
            .track_liveset(true) // also record frees, for leak views
            .build(),
    )
    .install(Dial9Handle::current())
    .expect("install memory profiler");

    let handle = Dial9TokioHandle::current();
    let mut tasks = Vec::new();
    for _ in 0..200 {
        tasks.push(handle.spawn(allocate_some()));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    for t in tasks {
        t.await.unwrap();
    }

    println!("\n✓ Trace with allocation events written to: {TRACE_DIR}");
    println!("  View: cargo run --package dial9-viewer -- --local-dir {TRACE_DIR}");
}
