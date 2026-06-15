//! Generate traces with common misconfigurations for testing the diagnostic skill.
//!
//! Usage:
//!   cargo run --release --features cpu-profiling --example generate_diagnostic_traces -- <output-dir>
//!
//! Produces:
//!   <output-dir>/no-wake-events/    — tasks spawned via tokio::spawn (not instrumented)
//!   <output-dir>/good/              — properly configured trace for comparison
//!   <output-dir>/no-sched-events/   — no off-CPU scheduling samples
//!
//! Note: "missing frame pointers" and "missing debug symbols" require building
//! with different RUSTFLAGS, so they are handled by the shell script wrapper.

use dial9_tokio_telemetry::telemetry::{
    Dial9TokioHandle, DiskWriter, TracedRuntime,
    cpu_profile::{CpuProfilingConfig, SchedEventConfig},
};
use std::path::PathBuf;
use std::time::Duration;

#[inline(never)]
fn burn_cpu(duration: Duration) {
    let start = std::time::Instant::now();
    let mut x: u64 = 1;
    while start.elapsed() < duration {
        for _ in 0..1000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
    }
}

async fn cpu_task(_id: usize) {
    for _ in 0..5 {
        burn_cpu(Duration::from_millis(20));
        tokio::task::yield_now().await;
    }
}

/// Generate a trace where tasks are NOT instrumented (no wake events).
/// Uses tokio::spawn instead of Dial9TokioHandle::spawn.
fn generate_no_wake_events(dir: &PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let trace_path = dir.join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = DiskWriter::new(&trace_path, 4 * 1024, 50 * 1024 * 1024).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(999))
        .with_trace_path(&trace_path)
        .with_worker_poll_interval(Duration::from_millis(50))
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        // Deliberately use tokio::spawn — NOT Dial9TokioHandle::spawn
        let tasks: Vec<_> = (0..50).map(|i| tokio::spawn(cpu_task(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    drop(runtime);
    guard
        .graceful_shutdown(Duration::from_secs(10))
        .expect("graceful shutdown");
}

/// Generate a fully-configured "good" trace for comparison.
fn generate_good_trace(dir: &PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let trace_path = dir.join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = DiskWriter::new(&trace_path, 4 * 1024, 50 * 1024 * 1024).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(999))
        .with_sched_events(SchedEventConfig::default())
        .with_trace_path(&trace_path)
        .with_worker_poll_interval(Duration::from_millis(50))
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        let handle = Dial9TokioHandle::current();
        let tasks: Vec<_> = (0..50).map(|i| handle.spawn(cpu_task(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    drop(runtime);
    guard
        .graceful_shutdown(Duration::from_secs(10))
        .expect("graceful shutdown");
}

/// Generate a trace with CPU profiling but NO sched events.
fn generate_no_sched_events(dir: &PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let trace_path = dir.join("trace.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = DiskWriter::new(&trace_path, 4 * 1024, 50 * 1024 * 1024).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(999))
        // Deliberately omit .with_sched_events()
        .with_trace_path(&trace_path)
        .with_worker_poll_interval(Duration::from_millis(50))
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        let handle = Dial9TokioHandle::current();
        let tasks: Vec<_> = (0..50).map(|i| handle.spawn(cpu_task(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    drop(runtime);
    guard
        .graceful_shutdown(Duration::from_secs(10))
        .expect("graceful shutdown");
}

fn main() {
    let output_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/tmp/dial9-diagnostic-traces".to_string()),
    );

    eprintln!(
        "Generating diagnostic traces in {}...",
        output_dir.display()
    );

    eprintln!("  → no-wake-events (tasks not instrumented)");
    generate_no_wake_events(&output_dir.join("no-wake-events"));

    eprintln!("  → no-sched-events (schedule profiling disabled)");
    generate_no_sched_events(&output_dir.join("no-sched-events"));

    eprintln!("  → good (fully configured)");
    generate_good_trace(&output_dir.join("good"));

    eprintln!("Done. Traces at: {}", output_dir.display());
}
