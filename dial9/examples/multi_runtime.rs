//! Multiple named runtimes sharing a single recorder.
//!
//! A common pattern is to run separate runtimes for different workload types
//! (e.g. request handling vs background I/O). This example builds a recorder and
//! attaches two named runtimes to it with `attach_runtime_with`, so all workers
//! appear in a single trace file with their runtime names in the segment
//! metadata. Requests run on one runtime and push their flush work onto the
//! other with `dial9::spawn_in`, so the trace attributes each to its own
//! runtime.
//!
//! Usage:
//!   cargo run --example multi_runtime
//!
//! After running, inspect the trace:
//!   cargo run --example analyze_trace -- /tmp/multi_runtime/trace.0.bin

use dial9::{DiskBuffer, RecorderTokioExt, TokioAttachOptions, recorder};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let trace_dir = "/tmp/multi_runtime";
    let _ = std::fs::create_dir_all(trace_dir);

    let writer = DiskBuffer::builder()
        .base_path(trace_dir)
        .max_file_size(1024 * 1024)
        .max_total_size(5 * 1024 * 1024)
        .build()?;

    let recorder = recorder(writer).build();

    // Primary runtime for request handling.
    let (recorder, main_rt) = recorder.attach_runtime_with(
        TokioAttachOptions::builder().runtime_name("main").build(),
        |t| {
            t.worker_threads(2);
        },
    )?;

    // Secondary runtime for background I/O, sharing the same recorder.
    let (recorder, io_rt) = recorder.attach_runtime_with(
        TokioAttachOptions::builder().runtime_name("io").build(),
        |t| {
            t.worker_threads(2);
        },
    )?;

    println!("Running workload on two named runtimes...");

    // Requests run on the main runtime and hand their write-behind to the I/O
    // runtime. `dial9::spawn` targets the runtime you are already on.
    // `dial9::spawn_in` targets a specific one, so the flush is instrumented as
    // work on `runtime.io` even though it was spawned from a `runtime.main`
    // worker.
    let io_handle = io_rt.handle().clone();
    main_rt.block_on(async move {
        let mut requests = Vec::new();
        for i in 0..20 {
            let io = io_handle.clone();
            requests.push(dial9::spawn(async move {
                // Simulate request processing: some CPU work + async I/O.
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(5)).await;
                println!("  [main] request {i} handled");

                dial9::spawn_in(&io, async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    println!("  [io]   request {i} flushed");
                })
                .await
                .unwrap();
            }));
        }
        for request in requests {
            request.await.unwrap();
        }
    });

    println!("All tasks completed.");

    // Drop the runtimes before shutdown so worker threads flush their buffers.
    drop(io_rt);
    drop(main_rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

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
