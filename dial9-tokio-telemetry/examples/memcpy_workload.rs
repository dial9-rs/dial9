//! Example: workload that spends significant time in memcpy due to moving large structs.
//!
//! This demonstrates how CPU profiling in dial9 can reveal time spent copying
//! memory. The workload moves a large (64 KiB) struct through a series of
//! function calls and collections, forcing the compiler to emit memcpy/memmove.
//!
//! Run with:
//!   RUSTFLAGS="--cfg tokio_unstable -C force-frame-pointers=yes" cargo run --release --features cpu-profiling --example memcpy_workload
//!
//! You may need:
//!   echo 2 | sudo tee /proc/sys/kernel/perf_event_paranoid

use dial9_tokio_telemetry::telemetry::{
    DiskWriter, TelemetryHandle, TracedRuntime, cpu_profile::CpuProfilingConfig,
};
use std::hint::black_box;
use std::time::Duration;

/// A large struct (64 KiB) that forces memcpy when moved.
#[derive(Clone)]
struct BigStruct {
    data: [u8; 64 * 1024],
}

impl BigStruct {
    fn new(seed: u8) -> Self {
        Self {
            data: [seed; 64 * 1024],
        }
    }

    /// Consume self by value (forces a move/copy) and return a transformed version.
    fn transform(mut self) -> Self {
        for chunk in self.data.chunks_mut(64) {
            chunk[0] = chunk[0].wrapping_add(1);
        }
        self
    }
}

/// Accepts and returns by value, forcing moves at each call boundary.
#[inline(never)]
fn pass_through(s: BigStruct) -> BigStruct {
    black_box(s)
}

/// Shuffles big structs through a Vec, causing repeated memcpy on insert/remove.
#[inline(never)]
fn churn_vec(iterations: usize) {
    let mut items: Vec<BigStruct> = (0..8).map(|i| BigStruct::new(i)).collect();

    for i in 0..iterations {
        // Remove from front (shifts all elements left = memcpy)
        let item = items.remove(0);
        // Transform through pass-by-value chain
        let item = pass_through(item);
        let item = item.transform();
        let item = pass_through(item);
        // Insert back at a position that forces shifting
        let pos = i % items.len().max(1);
        items.insert(pos, item);
    }

    black_box(&items);
}

async fn memcpy_task(id: usize) {
    for _ in 0..5 {
        // Each iteration churns large structs, spending time in memcpy
        churn_vec(200);
        tokio::task::yield_now().await;
    }
    eprintln!("Task {id} done");
}

fn main() {
    let trace_base = "memcpy_workload_trace.bin";

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = DiskWriter::builder()
        .base_path(trace_base)
        .max_file_size(64 * 1024 * 1024)
        .max_total_size(256 * 1024 * 1024)
        .build()
        .unwrap();

    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(trace_base)
        .with_task_tracking(true)
        .with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(999))
        .build_and_start(builder, writer)
        .unwrap();

    eprintln!("Running memcpy-heavy workload with CPU profiling at 999 Hz...");

    runtime.block_on(async {
        let handle = TelemetryHandle::current();
        let tasks: Vec<_> = (0..50).map(|i| handle.spawn(memcpy_task(i))).collect();

        for task in tasks {
            let _ = task.await;
        }
    });

    drop(runtime);

    eprintln!("Waiting for background worker to symbolize trace (up to 30s)...");
    if let Err(e) = guard.graceful_shutdown(Duration::from_secs(30)) {
        eprintln!("Worker shutdown warning: {e}");
    }

    eprintln!("Done. Trace written to memcpy_workload_trace.*.bin");
    eprintln!("View with: https://dial9-tokio-telemetry.netlify.app/");
}
