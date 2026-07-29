//! Example: conditionally enable dial9 telemetry via an environment variable.
//!
//! A common pattern is to run with telemetry in staging or on-demand in
//! production, while keeping a plain tokio runtime in dev. The `config`
//! function checks `ENABLE_DIAL9`: when the var is set it builds a recording
//! runtime, otherwise it returns a disabled recorder ([`recorder_disabled`]) and
//! a plain tokio runtime.
//!
//! [`recorder_disabled`]: dial9::recorder_disabled
//!
//! Run with telemetry enabled:
//! ```sh
//! ENABLE_DIAL9=1 cargo run --example conditionally_enable
//! ```
//!
//! Run with telemetry disabled (plain tokio runtime):
//! ```sh
//! cargo run --example conditionally_enable
//! ```

use std::io;
use std::time::Duration;

use dial9::Dial9Handle;
use dial9::{AttachedRuntime, Dial9HandleTokioExt, DiskBuffer, TokioAttachOptions};

fn my_config() -> io::Result<AttachedRuntime> {
    let recorder = if std::env::var("ENABLE_DIAL9").is_ok() {
        let writer = DiskBuffer::builder()
            .base_path("conditionally_enable_trace")
            .max_file_size(64 * 1024 * 1024)
            .max_total_size(256 * 1024 * 1024)
            .build();
        dial9::recorder_or_disabled(writer).build()
    } else {
        dial9::recorder_disabled()
    };

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();

    let runtime = recorder.handle().attach_tokio_runtime(
        builder,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    )?;
    Ok((recorder, runtime))
}

async fn cpu_work(iterations: u64) -> u64 {
    let mut result = 0u64;
    for i in 0..iterations {
        result = result.wrapping_add(i.wrapping_mul(i));
    }
    result
}

async fn mixed_task(id: usize) {
    for i in 0..10 {
        if i % 3 == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        } else {
            cpu_work(100_000).await;
        }
        tokio::task::yield_now().await;
    }
    println!("Task {id} completed");
}

#[dial9::main(config = my_config)]
async fn main() {
    let telemetry_enabled = Dial9Handle::current().is_enabled();
    println!(
        "Running workload (telemetry {})...",
        if telemetry_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );

    // `handle.spawn` records wake events when telemetry is enabled and
    // falls through to plain `tokio::spawn` when it is disabled.
    let tasks: Vec<_> = (0..50).map(|i| dial9::spawn(mixed_task(i))).collect();

    for task in tasks {
        let _ = task.await;
    }

    if telemetry_enabled {
        println!("All tasks completed — trace written to conditionally_enable_trace/trace.*.bin");
    } else {
        println!("All tasks completed — set ENABLE_DIAL9=1 to enable tracing");
    }
}
