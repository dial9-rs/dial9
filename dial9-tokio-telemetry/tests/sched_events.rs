//! Integration test: sched event capture via per-thread perf profiling.

#![cfg(all(feature = "cpu-profiling", target_os = "linux"))]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::InMemoryWriter;
use dial9_tokio_telemetry::telemetry::analysis_events::{CpuSampleSource, Dial9Event, WorkerId};

#[test]
fn sched_events_capture_context_switches() {
    use dial9_tokio_telemetry::telemetry::TracedRuntime;
    use dial9_tokio_telemetry::telemetry::cpu_profile::SchedEventConfig;
    use std::time::Duration;

    let (capture, batches) = capture_processor();

    let num_workers = 2u64;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers as usize).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_sched_events(SchedEventConfig::default())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..num_workers * 2 {
            handles.push(tokio::spawn(async {
                std::thread::sleep(Duration::from_millis(10));
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);

    let worker_sched_samples: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(e, Dial9Event::CpuSampleEvent(s)
            if s.worker_id < WorkerId(num_workers) && s.source == CpuSampleSource::SchedEvent)
        })
        .collect();
    assert!(
        !worker_sched_samples.is_empty(),
        "expected CpuSample events with source=SchedEvent attributed to workers"
    );

    // No samples should have CpuProfile source (we didn't enable cpu profiling)
    let cpu_profile_samples = events
        .iter()
        .filter(|e| {
            matches!(e, Dial9Event::CpuSampleEvent(s)
            if s.source == CpuSampleSource::CpuProfile)
        })
        .count();
    assert_eq!(cpu_profile_samples, 0, "should have no CpuProfile samples");
}

/// With `sampling_interval(10)`, perf records ~1/10 of the worker threads'
/// context switches. Rather than compare two independent runs (whose total
/// switch counts vary, making the ratio flaky), we run once and compare the
/// emitted sample count against the kernel's own per-worker context-switch
/// counter read from `/proc`. Numerator and denominator come from the same
/// threads in the same run, so the ~10x relationship holds deterministically.
#[test]
fn sched_events_sampling_reduces_count() {
    use dial9_tokio_telemetry::telemetry::TracedRuntime;
    use dial9_tokio_telemetry::telemetry::cpu_profile::SchedEventConfig;
    use std::collections::HashSet;
    use std::time::Duration;

    const PERIOD: u64 = 10;
    let num_workers = 2u64;

    let (capture, batches) = capture_processor();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers as usize).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_sched_events(SchedEventConfig::default().sampling_interval(PERIOD))
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    // Baseline switch counts for all current threads (workers already spawned).
    let before = common::snapshot_task_switches();

    runtime.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..num_workers * 20 {
            handles.push(tokio::spawn(async {
                for _ in 0..5 {
                    std::thread::sleep(Duration::from_millis(2));
                    tokio::task::yield_now().await;
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    // Snapshot again while the worker threads are still alive.
    let after = common::snapshot_task_switches();

    drop(runtime);
    guard.graceful_shutdown(Duration::from_secs(5)).unwrap();

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);

    // Worker sched samples and the set of worker tids that produced them.
    let mut worker_tids = HashSet::new();
    let records = events
        .iter()
        .filter(|e| {
            matches!(e, Dial9Event::CpuSampleEvent(s)
            if s.worker_id < WorkerId(num_workers) && s.source == CpuSampleSource::SchedEvent)
        })
        .inspect(|e| {
            if let Dial9Event::CpuSampleEvent(s) = e {
                worker_tids.insert(s.tid);
            }
        })
        .count();

    // Ground-truth context switches on exactly those sampled worker threads.
    let total: u64 = worker_tids
        .iter()
        .map(|tid| after.get(tid).copied().unwrap_or(0) - before.get(tid).copied().unwrap_or(0))
        .sum();

    assert!(
        total > 100,
        "workload produced too few worker context switches to test ({total})"
    );

    let expected = total as f64 / PERIOD as f64;
    let ratio = records as f64 / expected;
    assert!(
        (0.8..=1.2).contains(&ratio),
        "expected sampled records (~total/{PERIOD}); records={records}, \
         total={total}, expected~{expected:.0}, ratio={ratio:.2}"
    );
}
