//! Verify that CPU profile sample timestamps align with wall-clock timestamps,
//! and that samples are attributed to the correct worker thread.

#![cfg(all(feature = "cpu-profiling", target_os = "linux"))]

mod common;

use common::{BytesCapturingWriter, decode_all};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
enum CpuEvent {
    CpuSampleEvent {
        timestamp_ns: u64,
        worker_id: u64,
        tid: u32,
        source: u8,
        thread_name: Option<String>,
    },
    PollStartEvent {
        timestamp_ns: u64,
        worker_id: u64,
    },
    PollEndEvent {
        timestamp_ns: u64,
        worker_id: u64,
    },
    #[serde(other)]
    Other,
}

const SOURCE_CPU_PROFILE: u8 = 0;
const WORKER_UNKNOWN: u64 = 255;
const WORKER_BLOCKING: u64 = 254;

#[test]
fn cpu_sample_timestamps_align_with_wall_clock() {
    let _ = tracing_subscriber::fmt::try_init();
    use dial9_tokio_telemetry::telemetry::TracedRuntime;
    use dial9_tokio_telemetry::telemetry::clock_monotonic_ns;
    use dial9_tokio_telemetry::telemetry::cpu_profile::CpuProfilingConfig;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let (writer, batches) = BytesCapturingWriter::new();

    let num_workers = 2u64;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers as usize).enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(999))
        .build_and_start(builder, writer)
        .unwrap();

    let _trace_start = guard.start_time();
    let burn_windows: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));

    runtime.block_on(async {
        for _ in 0..3u64 {
            let windows = burn_windows.clone();
            tokio::spawn(async move {
                std::thread::sleep(Duration::from_millis(150));
                let before = clock_monotonic_ns();
                burn_cpu(Duration::from_millis(80));
                let after = clock_monotonic_ns();
                std::thread::sleep(Duration::from_millis(150));
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
    let events: Vec<CpuEvent> = decode_all(&b);
    let windows = burn_windows.lock().unwrap();

    // Extract CPU samples
    let cpu_samples: Vec<(u64, u64)> = events
        .iter()
        .filter_map(|e| match e {
            CpuEvent::CpuSampleEvent {
                timestamp_ns,
                worker_id,
                source,
                ..
            } if *source == SOURCE_CPU_PROFILE => Some((*timestamp_ns, *worker_id)),
            _ => None,
        })
        .collect();

    let cpu_ts: Vec<u64> = cpu_samples.iter().map(|&(t, _)| t).collect();
    assert!(!cpu_ts.is_empty(), "expected CPU profile samples");

    // Build poll intervals: (poll_start_ns, poll_end_ns, worker_id)
    let mut poll_intervals: Vec<(u64, u64, u64)> = Vec::new();
    {
        let mut open: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
        for event in &events {
            match event {
                CpuEvent::PollStartEvent {
                    timestamp_ns,
                    worker_id,
                } => {
                    open.insert(*worker_id, *timestamp_ns);
                }
                CpuEvent::PollEndEvent {
                    timestamp_ns,
                    worker_id,
                } => {
                    if let Some(start) = open.remove(worker_id) {
                        poll_intervals.push((start, *timestamp_ns, *worker_id));
                    }
                }
                _ => {}
            }
        }
    }

    let slack_ns = 1_000_000u64; // 1 ms

    for (i, &(burn_start, burn_end)) in windows.iter().enumerate() {
        let in_window: Vec<(u64, u64)> = cpu_samples
            .iter()
            .filter(|&&(t, _)| t >= burn_start.saturating_sub(slack_ns) && t <= burn_end + slack_ns)
            .copied()
            .collect();

        let closest = cpu_ts
            .iter()
            .map(|&t| {
                if t < burn_start {
                    burn_start - t
                } else {
                    t.saturating_sub(burn_end)
                }
            })
            .min()
            .unwrap_or(u64::MAX);

        eprintln!(
            "burn window {i}: [{burn_start}..{burn_end}] ({}ms), {} samples in window, closest: {}us",
            (burn_end - burn_start) / 1_000_000,
            in_window.len(),
            closest / 1_000,
        );

        assert!(
            !in_window.is_empty(),
            "burn window {i} ({}ms) had no CPU samples within {slack_ns}ns slack.\n\
             closest was {closest}ns away.\n\
             timestamps (first 20): {:?}",
            (burn_end - burn_start) / 1_000_000,
            &cpu_ts[..cpu_ts.len().min(20)]
        );

        // Worker attribution
        let owning_poll = poll_intervals
            .iter()
            .find(|&&(ps, pe, _)| ps <= burn_start + slack_ns && pe + slack_ns >= burn_end);

        if let Some(&(ps, pe, expected_worker)) = owning_poll {
            eprintln!("  burn window {i} owned by worker {expected_worker} (poll [{ps}..{pe}])");
            for &(t, w) in &in_window {
                if w == WORKER_UNKNOWN {
                    continue;
                }
                assert_eq!(
                    w, expected_worker,
                    "burn window {i}: CPU sample at {t}ns was attributed to worker \
                     {w} but the task was running on worker {expected_worker}."
                );
            }
        } else {
            panic!(
                "burn window {i} [{burn_start}..{burn_end}] \
                 could not be matched to a poll interval"
            );
        }
    }

    // Global: ≥90% of all CPU samples must fall within a burn window
    let total = cpu_ts.len();
    let in_any_window = cpu_ts
        .iter()
        .filter(|&&t| {
            windows
                .iter()
                .any(|&(s, e)| t >= s.saturating_sub(slack_ns) && t <= e + slack_ns)
        })
        .count();
    let pct = in_any_window as f64 / total as f64 * 100.0;
    eprintln!("{in_any_window}/{total} samples in burn windows ({pct:.1}%)");
    assert!(
        pct >= 90.0,
        "only {pct:.1}% of CPU samples fell within burn windows (expected >=90%)"
    );

    // All samples must lie within the total test duration
    let now = clock_monotonic_ns();
    for &t in &cpu_ts {
        assert!(
            t <= now + slack_ns,
            "CPU sample timestamp {t}ns exceeds current time {now}ns"
        );
    }
}

fn burn_cpu(duration: std::time::Duration) {
    let start = std::time::Instant::now();
    let mut x: u64 = 1;
    while start.elapsed() < duration {
        for _ in 0..1000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
    }
}

/// Verify that `ThreadNameDef` events are emitted for non-worker threads.
#[test]
fn thread_name_attribution_for_external_and_blocking_threads() {
    let _ = tracing_subscriber::fmt::try_init();
    use dial9_tokio_telemetry::telemetry::TracedRuntime;
    use dial9_tokio_telemetry::telemetry::cpu_profile::CpuProfilingConfig;
    use std::time::Duration;

    let (writer, batches) = BytesCapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .worker_threads(2)
        .thread_name("test-traced-runtime")
        .enable_all();

    let (runtime, guard) = TracedRuntime::builder()
        .with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(999))
        .build_and_start(builder, writer)
        .unwrap();

    // std::thread with a known name
    let ext_handle = std::thread::Builder::new()
        .name("my-ext-thread".into())
        .spawn(|| burn_cpu(Duration::from_millis(400)))
        .unwrap();

    // spawn_blocking
    let blocking_handle = runtime.spawn(async {
        tokio::task::spawn_blocking(|| burn_cpu(Duration::from_millis(400)));
        tokio::task::spawn_blocking(|| {
            let tid = nix::unistd::gettid().as_raw() as u32;
            burn_cpu(Duration::from_millis(400));
            tid
        })
        .await
        .unwrap()
    });

    ext_handle.join().unwrap();
    let blocking_tid = runtime.block_on(async {
        let tid = blocking_handle.await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        tid
    });

    drop(runtime);
    guard.graceful_shutdown(Duration::from_secs(1)).unwrap();

    let b = batches.lock().unwrap();
    let events: Vec<CpuEvent> = decode_all(&b);

    // Collect thread names from CpuSample events
    let thread_defs: Vec<(u32, &str)> = events
        .iter()
        .filter_map(|e| match e {
            CpuEvent::CpuSampleEvent {
                tid, thread_name, ..
            } => thread_name.as_deref().map(|name| (*tid, name)),
            _ => None,
        })
        .collect();

    let unique_defs: std::collections::HashMap<u32, &str> = thread_defs.iter().copied().collect();
    eprintln!("Thread names from CpuSample events: {unique_defs:?}");

    // Verify the external thread name appears
    let ext_def = thread_defs
        .iter()
        .find(|(_, name)| *name == "my-ext-thread");
    assert!(
        ext_def.is_some(),
        "expected CpuSample with thread_name 'my-ext-thread', got: {unique_defs:?}"
    );
    let ext_tid = ext_def.unwrap().0;

    // Verify the blocking thread has the expected name
    let blocking_name = unique_defs.get(&blocking_tid);
    eprintln!("Blocking thread tid={blocking_tid}, name={blocking_name:?}");
    assert!(
        blocking_name.is_some_and(|n| n.starts_with("test-traced-run")),
        "expected blocking thread name starting with 'test-traced-run', got: {blocking_name:?}"
    );

    // Verify CpuSamples exist for both tids with expected worker ids
    let ext_samples: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(e, CpuEvent::CpuSampleEvent { tid, worker_id, .. }
            if *tid == ext_tid && *worker_id == WORKER_UNKNOWN)
        })
        .collect();
    assert!(
        !ext_samples.is_empty(),
        "expected CPU samples for external thread tid={ext_tid}"
    );

    let blocking_samples: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(e, CpuEvent::CpuSampleEvent { tid, worker_id, .. }
            if *tid == blocking_tid && *worker_id == WORKER_BLOCKING)
        })
        .collect();
    assert!(
        !blocking_samples.is_empty(),
        "expected CPU samples for blocking thread tid={blocking_tid}"
    );
}
