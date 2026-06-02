//! Example: traced runtime with CPU profiling enabled.
//!
//! Runs a workload with some CPU-heavy polls, then reads back the trace
//! and prints any CpuSample events found.
//!
//! Run with:
//!   RUSTFLAGS="--cfg tokio_unstable -C force-frame-pointers=yes" cargo run --release --features cpu-profiling --example cpu_profile_workload
//!
//! You may need:
//!   echo 2 | sudo tee /proc/sys/kernel/perf_event_paranoid

use dial9_tokio_telemetry::telemetry::{
    DiskWriter, TracedRuntime, cpu_profile::CpuProfilingConfig,
};
use dial9_trace_format::decoder::Decoder;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum CpuEvent {
    CpuSampleEvent {
        timestamp_ns: u64,
        worker_id: u64,
        source: u8,
        callchain: Vec<u64>,
    },
    PollStartEvent {
        timestamp_ns: u64,
    },
    #[serde(other)]
    Other,
}

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

async fn cpu_heavy_task(id: usize) {
    for _ in 0..5 {
        burn_cpu(Duration::from_millis(20));
        tokio::task::yield_now().await;
    }
    eprintln!("Task {id} done");
}

fn main() {
    let trace_base = "cpu_profile_trace.bin";
    let segment_path = "cpu_profile_trace.0.bin";

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(4).enable_all();

    let writer = DiskWriter::builder()
        .base_path(trace_base)
        .max_file_size(1024 * 1024 * 20)
        .max_total_size(1024 * 1024 * 100)
        .build()
        .unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_trace_path(trace_base)
        .with_task_tracking(true)
        .with_cpu_profiling(CpuProfilingConfig::default())
        .build_and_start(builder, writer)
        .unwrap();

    eprintln!("Running workload with CPU profiling at 99 Hz...");
    runtime.block_on(async {
        let tasks: Vec<_> = (0..200).map(|i| tokio::spawn(cpu_heavy_task(i))).collect();
        for task in tasks {
            let _ = task.await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    drop(runtime);

    eprintln!("Waiting for background worker to symbolize trace (up to 30s)...");
    if let Err(e) = guard.graceful_shutdown(Duration::from_secs(30)) {
        eprintln!("Worker shutdown warning: {e}");
    }

    eprintln!("\n=== Reading trace from {segment_path} ===");
    let data = std::fs::read(segment_path).unwrap();
    let mut decoder = Decoder::new(&data).unwrap();

    let mut cpu_samples = 0usize;
    let mut polls = 0usize;
    let mut samples_by_worker: std::collections::HashMap<u64, usize> =
        std::collections::HashMap::new();

    decoder
        .for_each_event(|raw| {
            let ev: CpuEvent = raw.deserialize().expect("deserialize");
            match &ev {
                CpuEvent::CpuSampleEvent {
                    worker_id,
                    callchain,
                    timestamp_ns,
                    source,
                } => {
                    cpu_samples += 1;
                    *samples_by_worker.entry(*worker_id).or_default() += 1;
                    if cpu_samples <= 10 {
                        eprintln!(
                            "  CpuSample: worker={worker_id} t={timestamp_ns}ns source={source} frames={}",
                            callchain.len()
                        );
                        for (i, addr) in callchain.iter().take(8).enumerate() {
                            eprintln!("    [{i}] {addr:#x}");
                        }
                    }
                }
                CpuEvent::PollStartEvent { .. } => polls += 1,
                CpuEvent::Other => {}
            }
        })
        .unwrap();

    eprintln!("\nPoll starts: {polls}");
    eprintln!("CPU samples: {cpu_samples}");
    for (worker, count) in &samples_by_worker {
        eprintln!("  worker {worker}: {count} samples");
    }
    if cpu_samples == 0 {
        eprintln!("\nNo CPU samples collected! Check:");
        eprintln!("  - perf_event_paranoid: cat /proc/sys/kernel/perf_event_paranoid");
        eprintln!("  - frame pointers: RUSTFLAGS=\"-C force-frame-pointers=yes\"");
    }
}
