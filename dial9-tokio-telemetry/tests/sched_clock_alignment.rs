//! Verify that sched event timestamps (from perf) align with wall-clock
//! timestamps from `clock_monotonic_ns()` (CLOCK_MONOTONIC).

#![cfg(all(feature = "cpu-profiling", target_os = "linux"))]

mod common;

use common::{BytesCapturingWriter, decode_all};
use dial9_tokio_telemetry::telemetry::analysis_events::{CpuSampleSource, Dial9Event, WorkerId};

#[test]
fn sched_event_timestamps_align_with_wall_clock() {
    use dial9_tokio_telemetry::telemetry::TracedRuntime;
    use dial9_tokio_telemetry::telemetry::clock_monotonic_ns;
    use dial9_tokio_telemetry::telemetry::cpu_profile::SchedEventConfig;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let (writer, batches) = BytesCapturingWriter::new();

    let num_workers = 2u64;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers as usize).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_sched_events(SchedEventConfig::default())
        .build_and_start(builder, writer)
        .unwrap();

    let _trace_start = guard.start_time();
    let sleep_windows: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));

    let sleep_duration = Duration::from_millis(1);
    let num_sleeps = 4u64;

    runtime.block_on(async {
        // Warmup
        for _ in 0..num_workers {
            tokio::spawn(async {
                std::thread::sleep(Duration::from_millis(10));
            })
            .await
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        for i in 0..num_sleeps {
            let windows = sleep_windows.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(i * 100)).await;
                let before = clock_monotonic_ns();
                std::thread::sleep(sleep_duration);
                let after = clock_monotonic_ns();
                windows.lock().unwrap().push((before, after));
            })
            .await
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    drop(runtime);
    drop(guard);

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);
    let windows = sleep_windows.lock().unwrap();

    // Collect sched event timestamps attributed to workers
    let sched_timestamps: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Dial9Event::CpuSampleEvent(s)
                if s.source == CpuSampleSource::SchedEvent
                    && s.worker_id < WorkerId(num_workers) =>
            {
                Some(s.timestamp_ns)
            }
            _ => None,
        })
        .collect();

    assert!(
        !sched_timestamps.is_empty(),
        "expected sched events attributed to workers"
    );

    let slack_ns = 1_000_000u64; // 1ms
    for (i, &(start, end)) in windows.iter().enumerate() {
        let in_window: Vec<u64> = sched_timestamps
            .iter()
            .filter(|&&t| t >= start.saturating_sub(slack_ns) && t <= end + slack_ns)
            .copied()
            .collect();
        let closest = sched_timestamps
            .iter()
            .map(|&t| {
                if t < start {
                    start - t
                } else {
                    t.saturating_sub(end)
                }
            })
            .min()
            .unwrap_or(u64::MAX);
        eprintln!(
            "window {i}: [{start}..{end}] ({}ms), {} events in window, closest distance: {}us",
            (end - start) / 1_000_000,
            in_window.len(),
            closest / 1_000,
        );
        assert!(
            !in_window.is_empty(),
            "sleep window {i} [{start}..{end}] ({}ms) had no sched event within {slack_ns}ns slack.\n\
             closest sched event was {closest}ns away.\n\
             sched timestamps (first 20): {:?}",
            (end - start) / 1_000_000,
            &sched_timestamps[..sched_timestamps.len().min(20)]
        );
    }

    // Verify no sched events have wildly wrong timestamps
    let now = clock_monotonic_ns();
    for &t in &sched_timestamps {
        assert!(
            t <= now + slack_ns,
            "sched event timestamp {t}ns exceeds current time {now}ns"
        );
    }
}
