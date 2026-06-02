//! Integration test: sched event capture via per-thread perf profiling.

#![cfg(all(feature = "cpu-profiling", target_os = "linux"))]

mod common;

use common::{BytesCapturingWriter, decode_all};
use dial9_tokio_telemetry::telemetry::analysis_events::{CpuSampleSource, Dial9Event, WorkerId};

#[test]
fn sched_events_capture_context_switches() {
    use dial9_tokio_telemetry::telemetry::TracedRuntime;
    use dial9_tokio_telemetry::telemetry::cpu_profile::SchedEventConfig;
    use std::time::Duration;

    let (writer, batches) = BytesCapturingWriter::new();

    let num_workers = 2u64;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers as usize).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_sched_events(SchedEventConfig::default())
        .build_and_start(builder, writer)
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
    drop(guard);

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

#[test]
fn sched_events_sampling_reduces_count() {
    use dial9_tokio_telemetry::telemetry::TracedRuntime;
    use dial9_tokio_telemetry::telemetry::cpu_profile::SchedEventConfig;
    use std::time::Duration;

    let count_sched_samples = |interval: Option<u64>| -> usize {
        let (writer, batches) = BytesCapturingWriter::new();

        let num_workers = 2;
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(num_workers).enable_all();

        let mut config = SchedEventConfig::default();
        if let Some(n) = interval {
            config = config.sampling_interval(n);
        }

        let (runtime, guard) = TracedRuntime::builder()
            .with_sched_events(config)
            .build_and_start(builder, writer)
            .unwrap();

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
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        drop(runtime);
        drop(guard);

        let b = batches.lock().unwrap();
        let events: Vec<Dial9Event> = decode_all(&b);
        events
            .iter()
            .filter(|e| {
                matches!(e, Dial9Event::CpuSampleEvent(s)
                if s.source == CpuSampleSource::SchedEvent)
            })
            .count()
    };

    let n_all = count_sched_samples(None);
    let n_sampled = count_sched_samples(Some(10));

    let ratio = n_all as f64 / n_sampled.max(1) as f64;
    assert!(
        ratio > 8.0 && ratio < 12.0,
        "expected ~10x ratio, got {ratio:.1}x (n_all={n_all}, n_sampled={n_sampled})"
    );
}
