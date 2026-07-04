//! Minimal dial9 + memory profiling example.
//!
//! Sets `Dial9Allocator` as the global allocator, enables memory profiling
//! on the running dial9 session, does some allocating work, and writes a trace
//! with `AllocEvent`s to disk.
//!
//!   cargo run -p memory-local

use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_tokio_telemetry::telemetry::{Dial9Handle, Dial9TokioHandle};
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

fn my_config() -> Dial9Config {
    let trace_path = format!("{TRACE_DIR}/trace.bin");
    Dial9Config::builder()
        .on_disk_buffer(&trace_path)
        .max_file_size(10_000_000)
        .max_total_size(50_000_000)
        .with_runtime(|r| r.with_task_tracking(true))
        .with_tokio(|t| {
            t.worker_threads(2);
        })
        .build_or_disabled()
}

#[dial9_tokio_telemetry::main(config = my_config)]
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
