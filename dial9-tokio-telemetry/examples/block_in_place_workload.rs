//! Example that exercises `tokio::task::block_in_place` to produce traces
//! with block-in-place gaps. Used to generate test fixtures for the JS
//! analysis layer's gap detection.
//!
//! Run:
//!   cargo run --example block_in_place_workload --features cpu-profiling
//!
//! Produces: `block_in_place_trace.bin` in the current directory.

use dial9_tokio_telemetry::telemetry::{DiskWriter, TracedRuntime};
use std::time::Duration;

/// CPU-intensive work that shows up in CPU profiles.
fn burn_cpu(millis: u64) {
    let start = std::time::Instant::now();
    let mut x: u64 = 1;
    while start.elapsed() < Duration::from_millis(millis) {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
    }
    std::hint::black_box(x);
}

/// Task that burns CPU, then calls block_in_place, then burns more CPU.
async fn task_with_block_in_place(id: usize) {
    // Phase 1: CPU work visible in profiles (attributed to this worker).
    burn_cpu(100);
    tokio::task::yield_now().await;

    // Phase 2: block_in_place — triggers worker handoff.
    tokio::task::block_in_place(|| {
        // Blocking work inside block_in_place.
        std::thread::sleep(Duration::from_millis(100));
    });

    // Phase 3: More CPU work after block_in_place returns.
    burn_cpu(100);
    tokio::task::yield_now().await;
    println!("Task {id} done");
}

/// Background CPU work to keep all workers busy and generating samples.
async fn background_burn(id: usize) {
    for _ in 0..10 {
        burn_cpu(20);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    println!("Background {id} done");
}

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = DiskWriter::builder()
        .base_path("block_in_place_trace.bin")
        .max_file_size(100 * 1024 * 1024)
        .max_total_size(500 * 1024 * 1024)
        .build()
        .unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path("block_in_place_trace.bin")
        .with_task_tracking(true)
        .with_cpu_profiling(Default::default())
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        // Background work on all workers to generate CPU samples.
        let bg: Vec<_> = (0..8).map(|i| tokio::spawn(background_burn(i))).collect();

        // Let background work establish CPU sample baseline.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Spawn tasks that call block_in_place.
        let bip: Vec<_> = (0..2)
            .map(|i| tokio::spawn(task_with_block_in_place(i)))
            .collect();

        for t in bip {
            let _ = t.await;
        }
        for t in bg {
            let _ = t.await;
        }

        // Final burst of CPU work to confirm attribution is correct after.
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(runtime);
    // Graceful shutdown seals the final segment and runs symbolization.
    guard.graceful_shutdown(Duration::from_secs(10)).ok();

    println!("Trace written to block_in_place_trace.*.bin");
}
