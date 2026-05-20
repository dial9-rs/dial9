//! Example that exercises `tokio::task::block_in_place` to produce traces
//! with block-in-place gaps. Used to generate test fixtures for the JS
//! analysis layer's gap detection.
//!
//! Run:
//!   cargo run --example block_in_place_workload --features cpu-profiling
//!
//! Produces: `block_in_place_trace.bin` in the current directory.

use dial9_tokio_telemetry::telemetry::{RotatingWriter, TracedRuntime};
use std::time::Duration;

/// Simulates a task that calls block_in_place to do synchronous I/O.
async fn task_with_block_in_place(id: usize) {
    // Do some async work first so the worker is clearly active.
    tokio::time::sleep(Duration::from_millis(5)).await;

    // block_in_place: the worker hands off its core to a replacement thread.
    tokio::task::block_in_place(|| {
        // Simulate blocking work (e.g., synchronous file I/O).
        std::thread::sleep(Duration::from_millis(50));
    });

    // More async work after block_in_place returns.
    tokio::time::sleep(Duration::from_millis(5)).await;
    println!("Task {id} done");
}

/// Normal async task that keeps workers busy between block_in_place calls.
async fn background_work() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn main() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = RotatingWriter::single_file("block_in_place_trace.bin").unwrap();
    let (runtime, _guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_cpu_profiling(Default::default())
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        // Spawn background work to keep workers active.
        let bg: Vec<_> = (0..4).map(|_| tokio::spawn(background_work())).collect();

        // Spawn tasks that call block_in_place.
        let bip: Vec<_> = (0..3)
            .map(|i| tokio::spawn(task_with_block_in_place(i)))
            .collect();

        for t in bip {
            let _ = t.await;
        }
        for t in bg {
            let _ = t.await;
        }
    });

    println!("Trace written to block_in_place_trace.bin");
}
