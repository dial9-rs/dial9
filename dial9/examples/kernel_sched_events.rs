//! Example: sched events with kernel stack frames.
//!
//! Captures context-switch callchains that include kernel frames, showing
//! exactly where in the kernel the thread was descheduled. Reads back the
//! trace and prints sample callchains so you can verify your setup.
//!
//! Run with:
//!   cargo run --release --features cpu-profiling --example kernel_sched_events
//!
//! Requirements:
//!   - perf_event_paranoid ≤ 1:  sudo sysctl kernel.perf_event_paranoid=1
//!
//! Example output (nanosleep descheduling a tokio worker):
//!
//!   __schedule                                    ← kernel
//!   schedule
//!   do_nanosleep
//!   hrtimer_nanosleep
//!   __x64_sys_nanosleep
//!   do_syscall_64
//!   entry_SYSCALL_64_after_hwframe
//!   __GI___nanosleep                              ← libc
//!   std::thread::sleep                            ← userspace
//!   kernel_sched_events::blocking_task::{{closure}}
//!   tokio::runtime::task::core::Core<T,S>::poll
//!   ...
//!   start_thread
//!
//! Example output (tokio worker parking on futex):
//!
//!   __schedule                                    ← kernel
//!   schedule
//!   futex_wait_queue_me
//!   futex_wait
//!   do_futex
//!   __x64_sys_futex
//!   do_syscall_64
//!   entry_SYSCALL_64_after_hwframe
//!   syscall                                       ← libc
//!   tokio::..::park::Inner::park_condvar          ← userspace
//!   tokio::..::worker::Context::park_internal
//!   ...
//!   start_thread

// Example prints the deprecated `CpuSampleEvent::worker_id` for illustration.
#![allow(deprecated)]

use dial9::analysis::analysis_events::{CpuSampleSource, Dial9Event};
use dial9::cpu::SchedEventConfig;
use dial9::{DiskBuffer, recorder};
use dial9::{RecorderBuilderTokioExt, RecorderPerfExt};
use dial9_trace_format::decoder::Decoder;
use std::time::Duration;

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

    let writer = DiskBuffer::single_file(&trace_base).unwrap();
    let traced = recorder(writer)
        .with_sched_events(
            SchedEventConfig::default()
                .sampling_interval(5)
                .include_kernel(true),
        )
        .with_tokio(|t| {
            t.worker_threads(2);
        })
        .with_task_tracking(true)
        .build()
        .unwrap();

    traced.runtime().block_on(async {
        let tasks: Vec<_> = (0..4).map(|i| tokio::spawn(blocking_task(i))).collect();
        for t in tasks {
            let _ = t.await;
        }
    });

    drop(traced);

    // Read back and print callchains
    eprintln!("\n=== Reading trace from {trace_read_path} ===");
    let data = std::fs::read(&trace_read_path).unwrap();
    let mut decoder = Decoder::new(&data).unwrap();

    let mut printed = 0;
    let mut total_samples = 0;

    decoder
        .for_each_event(|raw| {
            let ev: Dial9Event = raw.deserialize().expect("deserialize");
            if let Dial9Event::CpuSampleEvent(ref s) = ev {
                if s.source != CpuSampleSource::SchedEvent {
                    return;
                }
                total_samples += 1;
                if printed < 3 {
                    printed += 1;
                    eprintln!(
                        "\n--- SchedEvent sample #{printed} (worker {}) ---",
                        s.worker_id
                    );
                    for addr in &s.callchain {
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
