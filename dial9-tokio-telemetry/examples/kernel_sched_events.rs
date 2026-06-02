//! Example: sched events with kernel stack frames.
//!
//! Captures context-switch callchains that include kernel frames, showing
//! exactly where in the kernel the thread was descheduled.
//!
//! Run with:
//!   cargo run --release --features cpu-profiling --example kernel_sched_events
//!
//! Requirements:
//!   - perf_event_paranoid ≤ 1:  sudo sysctl kernel.perf_event_paranoid=1

use dial9_tokio_telemetry::telemetry::{DiskWriter, TracedRuntime, cpu_profile::SchedEventConfig};
use dial9_trace_format::decoder::Decoder;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum SchedSample {
    CpuSampleEvent {
        worker_id: u64,
        source: u8,
        callchain: Vec<u64>,
    },
    #[serde(other)]
    Other,
}

const SOURCE_SCHED_EVENT: u8 = 1;

async fn blocking_task(id: usize) {
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(10));
        tokio::task::yield_now().await;
    }
    eprintln!("Task {id} done");
}

fn main() {
    let trace_dir = "example-traces";
    std::fs::create_dir_all(trace_dir).unwrap();
    let trace_base = format!("{trace_dir}/kernel_sched_trace.bin");
    let trace_read_path = format!("{trace_dir}/kernel_sched_trace.0.bin");

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();

    let writer = DiskWriter::single_file(&trace_base).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_sched_events(
            SchedEventConfig::default()
                .sampling_interval(5)
                .include_kernel(true),
        )
        .build_and_start(builder, writer)
        .unwrap();

    runtime.block_on(async {
        let tasks: Vec<_> = (0..4).map(|i| tokio::spawn(blocking_task(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
    });

    drop(runtime);
    drop(guard);

    eprintln!("\n=== Reading trace from {trace_read_path} ===");
    let data = std::fs::read(&trace_read_path).unwrap();
    let mut decoder = Decoder::new(&data).unwrap();

    let mut printed = 0;
    let mut total_samples = 0;

    decoder
        .for_each_event(|raw| {
            let ev: SchedSample = raw.deserialize().expect("deserialize");
            if let SchedSample::CpuSampleEvent {
                worker_id,
                source,
                callchain,
            } = &ev
            {
                if *source != SOURCE_SCHED_EVENT {
                    return;
                }
                total_samples += 1;
                if printed < 3 {
                    printed += 1;
                    eprintln!("\n--- SchedEvent sample #{printed} (worker {worker_id}) ---");
                    for addr in callchain {
                        eprintln!("  {addr:#x}");
                    }
                }
            }
        })
        .unwrap();

    eprintln!("\nTotal sched event samples: {total_samples}");
    if total_samples == 0 {
        eprintln!("No samples! Check: sudo sysctl kernel.perf_event_paranoid=1");
    }
}
