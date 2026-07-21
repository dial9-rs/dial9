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

// Example prints the deprecated `CpuSampleEvent::worker_id` for illustration.
#![allow(deprecated)]

use dial9::analysis::analysis_events::{CpuSampleSource, Dial9Event, WorkerId};
use dial9::cpu::CpuProfilingConfig;
use dial9::{DiskBuffer, recorder};
use dial9::{RecorderBuilderTokioExt, RecorderPerfExt};
use dial9_trace_format::decoder::Decoder;
use std::time::Duration;

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
        // This poll will show up as a long poll with CPU samples inside it
        burn_cpu(Duration::from_millis(20));
        tokio::task::yield_now().await;
    }
    eprintln!("Task {id} done");
}

fn main() {
    // base_path is a directory: the writer produces cpu_profile_trace/trace.0.bin,
    // which the background worker can detect, symbolize, and gzip-compress.
    let trace_dir = "cpu_profile_trace";
    let segment_path = "cpu_profile_trace/trace.0.bin";

    let writer = DiskBuffer::builder()
        .base_path(trace_dir)
        .max_file_size(1024 * 1024 * 20) // rotate after 20 MiB per file
        .max_total_size(1024 * 1024 * 100) // keep at most 100 MiB on disk
        .build()
        .unwrap();
    let traced = recorder(writer)
        .with_cpu_profiling(CpuProfilingConfig::default())
        .with_tokio(|t| {
            t.worker_threads(4);
        })
        .with_task_tracking(true)
        .build()
        .unwrap();

    eprintln!("Running workload with CPU profiling at 99 Hz...");
    traced.runtime().block_on(async {
        let tasks: Vec<_> = (0..200).map(|i| tokio::spawn(cpu_heavy_task(i))).collect();
        for task in tasks {
            let _ = task.await;
        }
        // Give the flush thread time to drain samples
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    // Graceful shutdown: flush + seal the segment, then wait for the background
    // worker to symbolize and gzip-compress it. Drop impl is a hard shutdown
    // (worker exits without draining), so we must use graceful_shutdown here.
    eprintln!("Waiting for background worker to symbolize trace (up to 30s)...");
    traced.graceful_shutdown(Duration::from_secs(30));

    // Read back and report
    eprintln!("\n=== Reading trace from {segment_path} ===");
    let data = std::fs::read(segment_path).unwrap();
    let mut decoder = Decoder::new(&data).unwrap();

    let mut cpu_samples = 0usize;
    let mut polls = 0usize;
    let mut samples_by_worker: std::collections::HashMap<WorkerId, usize> =
        std::collections::HashMap::new();

    decoder
        .for_each_event(|raw| {
            let ev: Dial9Event = raw.deserialize().expect("deserialize");
            match &ev {
                Dial9Event::CpuSampleEvent(e) if e.source == CpuSampleSource::CpuProfile => {
                    cpu_samples += 1;
                    *samples_by_worker.entry(e.worker_id).or_default() += 1;
                    if cpu_samples <= 10 {
                        eprintln!(
                            "  CpuSample: worker={} t={}ns source={:?} frames={}",
                            e.worker_id,
                            e.timestamp_ns,
                            e.source,
                            e.callchain.len()
                        );
                        for (i, addr) in e.callchain.iter().take(8).enumerate() {
                            eprintln!("    [{i}] {addr:#x}");
                        }
                    }
                }
                Dial9Event::PollStartEvent(_) => polls += 1,
                _ => {}
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
