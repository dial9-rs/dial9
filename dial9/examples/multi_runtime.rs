//! Multiple named runtimes sharing a single recorder.
//!
//! A common pattern is to run separate runtimes for different workload types
//! (e.g. request handling vs background I/O). This example builds a primary
//! runtime with `recorder(w).with_tokio(..).build()`, then attaches a second
//! one with `trace_runtime`, so all workers appear in a single trace file with
//! their runtime names in the segment metadata.
//!
//! Usage:
//!   cargo run --example multi_runtime
//!
//! After running, inspect the trace:
//!   cargo run --example analyze_trace -- /tmp/multi_runtime/trace.0.bin

use dial9::{DiskBuffer, RecorderBuilderTokioExt, recorder};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let trace_dir = "/tmp/multi_runtime";
    let _ = std::fs::create_dir_all(trace_dir);

    let writer = DiskBuffer::builder()
        .base_path(trace_dir)
        .max_file_size(1024 * 1024)
        .max_total_size(5 * 1024 * 1024)
        .build()?;

    // Primary runtime for request handling.
    let traced = recorder(writer)
        .with_tokio(|t| {
            t.worker_threads(2);
        })
        .with_runtime_name("main")
        .build()?;

    // Secondary runtime for background I/O, sharing the same recorder.
    let mut io_builder = tokio::runtime::Builder::new_multi_thread();
    io_builder.worker_threads(2).enable_all();
    let (io_rt, io_handle) = traced.trace_runtime("io").build(io_builder)?;

    println!("Running workload on two named runtimes...");

    // Request handling on the main runtime. Spawn through the handle
    // instead of tokio::spawn() for wake-event tracking.
    let main_handle = traced.handle();
    traced.runtime().block_on(async {
        let mut handles = Vec::new();
        for i in 0..20 {
            handles.push(main_handle.spawn(async move {
                // Simulate request processing: some CPU work + async I/O.
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(5)).await;
                tokio::task::yield_now().await;
                println!("  [main] request {i} done");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    // Background I/O on the second runtime.
    io_rt.block_on(async {
        let mut handles = Vec::new();
        for i in 0..10 {
            handles.push(io_handle.spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                tokio::task::yield_now().await;
                println!("  [io]   batch {i} flushed");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    println!("All tasks completed.");

    // Drop the attached runtime before shutdown so worker threads flush their buffers.
    drop(io_rt);
    traced.graceful_shutdown(Duration::from_secs(5));

    println!("\nTrace files in {trace_dir}/:");
    for entry in std::fs::read_dir(trace_dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        println!(
            "  {} ({} bytes)",
            entry.file_name().to_string_lossy(),
            meta.len()
        );
    }
    println!("\nThe trace viewer will show workers grouped by runtime name (main / io).");

    Ok(())
}
