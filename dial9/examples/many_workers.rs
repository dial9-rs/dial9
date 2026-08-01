//! Generate a trace with 48 workers for testing the viewer with many lanes.
//!
//! Usage:
//!   cargo run --example many_workers
//!
//! Then open the trace in the viewer:
//!   cargo run -p dial9-viewer -- serve --local-dir .

use std::io;
use std::time::Duration;

use dial9::{AttachedRuntime, Dial9HandleTokioExt, DiskBuffer, TokioAttachOptions};

fn my_config() -> io::Result<AttachedRuntime> {
    let writer = DiskBuffer::builder()
        .base_path("many_workers_trace")
        .max_file_size(64 * 1024 * 1024)
        .max_total_size(256 * 1024 * 1024)
        .build();
    let recorder = dial9::recorder_or_disabled(writer).build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(48);

    let runtime = recorder.handle().attach_tokio_runtime(
        builder,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    )?;
    Ok((recorder, runtime))
}

#[dial9::main(config = my_config)]
async fn main() {
    println!("Running workload with 48 workers...");
    let tasks: Vec<_> = (0..500)
        .map(|i| {
            dial9::spawn(async move {
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    // Small CPU work to generate poll events
                    let mut v = 0u64;
                    for j in 0..50_000u64 {
                        v = v.wrapping_add(j.wrapping_mul(j));
                    }
                    std::hint::black_box(v);
                    tokio::task::yield_now().await;
                }
                if i % 100 == 0 {
                    println!("Task {i} done");
                }
            })
        })
        .collect();

    for task in tasks {
        let _ = task.await;
    }

    println!("Trace written to many_workers_trace/trace.*.bin");
}
