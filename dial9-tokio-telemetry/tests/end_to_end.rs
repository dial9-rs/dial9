#![cfg_attr(not(tokio_unstable), allow(unused_imports))]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all, decode_file};
use dial9_tokio_telemetry::telemetry::analysis_events::{Dial9Event, WorkerId};
use dial9_tokio_telemetry::telemetry::{
    DiskBuffer, MemoryBuffer, RecorderPipelineExt, TokioAttachOptions, recorder, spawn,
};
use std::time::Duration;

/// Run a known workload on an attached runtime, read the trace back, and verify
/// basic consistency.
#[cfg(tokio_unstable)]
#[test]
fn end_to_end_trace_matches_workload_and_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let num_workers = 4;
    let total_tasks: usize = 2000;

    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    let rt = common::attach(&recorder, num_workers, TokioAttachOptions::default());

    let tokio_metrics = rt.block_on(async move {
        let mut handles = Vec::new();
        for _ in 0..total_tasks {
            handles.push(tokio::spawn(async {
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        tokio::runtime::Handle::current().metrics()
    });

    drop(rt);
    drop(recorder);

    let sealed_path = dir.path().join("trace.0.bin");
    let events: Vec<Dial9Event> = decode_file(&sealed_path);

    // Basic validation: poll starts == poll ends
    let poll_starts = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::PollStartEvent(_)))
        .count();
    let poll_ends = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::PollEndEvent(_)))
        .count();
    assert_eq!(
        poll_starts, poll_ends,
        "PollStart ({poll_starts}) != PollEnd ({poll_ends})"
    );

    // All active workers should appear
    let metrics_polls: Vec<u64> = (0..num_workers)
        .map(|w| tokio_metrics.worker_poll_count(w))
        .collect();
    let active_workers: Vec<usize> = (0..num_workers).filter(|&w| metrics_polls[w] > 0).collect();

    for &w in &active_workers {
        let worker_polls = events
            .iter()
            .filter(|e| matches!(e, Dial9Event::PollStartEvent(ev) if ev.worker_id == WorkerId(w as u64)))
            .count();
        assert!(
            worker_polls > 0,
            "worker {w} had {} tokio polls but 0 trace PollStart events",
            metrics_polls[w]
        );
    }

    // Timestamp monotonicity per worker
    let mut last_ts: Vec<Option<u64>> = vec![None; num_workers];
    for event in &events {
        let (ts, wid) = match event {
            Dial9Event::PollStartEvent(e) => (e.timestamp_ns, e.worker_id),
            Dial9Event::PollEndEvent(e) => (e.timestamp_ns, e.worker_id),
            Dial9Event::WorkerParkEvent(e) => (e.timestamp_ns, e.worker_id),
            Dial9Event::WorkerUnparkEvent(e) => (e.timestamp_ns, e.worker_id),
            _ => continue,
        };
        if wid.as_u64() >= num_workers as u64 {
            continue;
        }
        if let Some(prev) = last_ts[wid.as_u64() as usize] {
            assert!(
                ts >= prev,
                "timestamp regression on worker {wid}: {prev} -> {ts}"
            );
        }
        last_ts[wid.as_u64() as usize] = Some(ts);
    }

    // Park/unpark balance: each worker's parks and unparks should match within ±1
    let mut parks_per_worker: std::collections::HashMap<WorkerId, usize> =
        std::collections::HashMap::new();
    let mut unparks_per_worker: std::collections::HashMap<WorkerId, usize> =
        std::collections::HashMap::new();
    for event in &events {
        match event {
            Dial9Event::WorkerParkEvent(e) => {
                *parks_per_worker.entry(e.worker_id).or_default() += 1;
            }
            Dial9Event::WorkerUnparkEvent(e) => {
                *unparks_per_worker.entry(e.worker_id).or_default() += 1;
            }
            _ => {}
        }
    }
    for (&wid, &parks) in &parks_per_worker {
        let unparks = unparks_per_worker.get(&wid).copied().unwrap_or(0);
        let diff = (parks as i64 - unparks as i64).unsigned_abs();
        assert!(
            diff <= 1,
            "worker {wid}: park/unpark imbalance: parks={parks}, unparks={unparks}"
        );
    }
}

/// Regression test: TaskSpawn events emitted on the main thread (inside block_on)
/// must appear in the trace.
#[cfg(tokio_unstable)]
#[test]
fn task_spawn_events_from_main_thread_are_captured() {
    let (capture, batches) = capture_processor();

    const N: usize = 10;

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(
        &recorder,
        2,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );

    rt.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..N {
            handles.push(tokio::spawn(async {}));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);
    let task_spawn_count = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::TaskSpawnEvent(_)))
        .count();

    assert_eq!(
        task_spawn_count, N,
        "expected {N} TaskSpawn events from main thread, got {task_spawn_count}"
    );
}

/// Worker attribution: spawns that run on a worker carry that worker's id;
/// spawns from the `block_on` thread (which is not a worker) carry `None`.
#[cfg(tokio_unstable)]
#[test]
fn task_spawn_records_worker_id_only_when_spawned_on_a_worker() {
    use dial9_tokio_telemetry::telemetry::current_worker_id;
    use std::sync::{Arc, Mutex};

    let (capture, batches) = capture_processor();

    const OFF_WORKER: usize = 5;

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(
        &recorder,
        2,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );

    let worker_that_spawned: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let on_worker = worker_that_spawned.clone();

    rt.block_on(async move {
        let mut handles = Vec::new();
        // Spawned from the block_on thread, which is not a worker.
        for _ in 0..OFF_WORKER {
            handles.push(tokio::spawn(async {}));
        }
        handles.push(tokio::spawn(async move {
            // The task body runs on a worker, so this spawn does too.
            *on_worker.lock().unwrap() = Some(current_worker_id().as_u64());
            tokio::spawn(async {});
        }));
        for h in handles {
            h.await.unwrap();
        }
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let worker = worker_that_spawned
        .lock()
        .unwrap()
        .expect("task body must run on a worker");
    assert_ne!(worker, WorkerId::UNKNOWN.as_u64());

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);

    let mut on_workers = Vec::new();
    let mut off_workers = 0;
    for event in &events {
        if let Dial9Event::TaskSpawnEvent(ev) = event {
            match ev.worker_id {
                Some(w) => on_workers.push(w.as_u64()),
                None => off_workers += 1,
            }
        }
    }

    // The inner spawn (and only it) ran on a worker.
    assert_eq!(on_workers, vec![worker]);
    // OFF_WORKER empty tasks + the outer task, all spawned from block_on.
    assert_eq!(
        off_workers,
        OFF_WORKER + 1,
        "expected only the spawns made on a worker to carry a worker id"
    );
}

/// A spawn entering this runtime from a *worker of another runtime* records
/// `None`: the hook runs on the foreign worker, whose thread has a worker
/// index of its own, and mapping that index through this runtime's registry
/// would name a worker that never ran the spawn. The same test keeps one
/// spawn from a worker of this runtime as the control, so the discrimination
/// is pinned in both directions.
#[cfg(tokio_unstable)]
#[test]
fn task_spawn_from_a_foreign_runtime_worker_records_no_worker() {
    use dial9_tokio_telemetry::telemetry::current_worker_id;
    use std::sync::{Arc, Mutex};

    let (capture, batches) = capture_processor();

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(
        &recorder,
        1,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );
    let handle = rt.handle().clone();

    // A second, unrelated runtime: its workers are not this runtime's.
    let foreign = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build the foreign runtime");

    let worker_that_spawned: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let on_worker = worker_that_spawned.clone();

    rt.block_on(async move {
        // Spawned from the block_on thread: no worker.
        tokio::spawn(async move {
            // This body runs on one of this runtime's workers.
            *on_worker.lock().unwrap() = Some(current_worker_id().as_u64());
            tokio::spawn(async {});
        })
        .await
        .unwrap();
    });

    foreign.block_on(async {
        let handle = handle.clone();
        // This task runs on a worker of the foreign runtime; the spawn it
        // makes enters the recorded runtime from there.
        tokio::spawn(async move {
            handle.spawn(async {});
        })
        .await
        .unwrap();
    });

    drop(foreign);
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let worker = worker_that_spawned
        .lock()
        .unwrap()
        .expect("the control task body must run on a worker");
    assert_ne!(worker, WorkerId::UNKNOWN.as_u64());

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);

    let mut on_workers = Vec::new();
    let mut off_workers = 0;
    for event in &events {
        if let Dial9Event::TaskSpawnEvent(ev) = event {
            match ev.worker_id {
                Some(w) => on_workers.push(w.as_u64()),
                None => off_workers += 1,
            }
        }
    }

    // The control spawn on this runtime's worker is attributed to it; the
    // foreign worker's spawn is not. Without the runtime check the foreign
    // spawn would carry an id derived from the foreign worker's index.
    assert_eq!(on_workers, vec![worker]);
    assert_eq!(
        off_workers, 2,
        "expected the block_on spawn and the foreign-runtime spawn to be unattributed"
    );
}

#[cfg(tokio_unstable)]
#[test]
fn task_terminate_events_are_captured() {
    let (capture, batches) = capture_processor();

    const N: usize = 10;

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(
        &recorder,
        2,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );

    rt.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..N {
            handles.push(tokio::spawn(async {}));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);
    let terminate_count = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::TaskTerminateEvent(_)))
        .count();

    assert!(
        terminate_count >= N,
        "expected at least {N} TaskTerminate events, got {terminate_count}"
    );
}

#[test]
fn custom_event_appears_in_trace() {
    use dial9_trace_format::TraceEvent as TraceEventDerive;

    #[derive(TraceEventDerive)]
    struct MyCustomEvent {
        #[traceevent(timestamp)]
        timestamp_ns: u64,
        request_count: u32,
    }

    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    let rt = common::attach(&recorder, 2, TokioAttachOptions::default());

    let handle = recorder.handle().clone();
    rt.block_on(async move {
        for i in 0..5 {
            handle.record_event(MyCustomEvent {
                timestamp_ns: dial9_tokio_telemetry::telemetry::clock_monotonic_ns(),
                request_count: i,
            });
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(rt);
    drop(recorder);

    let sealed_path = dir.path().join("trace.0.bin");
    let data = std::fs::read(&sealed_path).unwrap();
    let mut decoder = dial9_trace_format::decoder::Decoder::new(&data).unwrap();
    let mut custom_count = 0u32;
    decoder
        .for_each_event(|ev| {
            if ev.name == "MyCustomEvent" {
                custom_count += 1;
            }
        })
        .unwrap();
    assert_eq!(custom_count, 5, "expected 5 MyCustomEvent events in trace");
}

// The spawn audit is precisely the instrumented-vs-raw distinction, and raw
// `tokio::spawn` tasks are only visible through tokio's spawn hook.
#[cfg(tokio_unstable)]
#[test]
fn spawn_audit_detects_uninstrumented_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");

    const RAW: usize = 5;
    const INSTRUMENTED: usize = 3;

    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    let rt = common::attach(
        &recorder,
        2,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );

    rt.block_on(async {
        let mut joins = Vec::new();

        for _ in 0..INSTRUMENTED {
            joins.push(spawn(async {}));
        }

        for _ in 0..RAW {
            joins.push(tokio::spawn(async {}));
        }

        for j in joins {
            j.await.unwrap();
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(rt);
    drop(recorder);

    let sealed_path = dir.path().join("trace.0.bin");
    let events: Vec<Dial9Event> = decode_file(&sealed_path);

    let instrumented_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Dial9Event::TaskSpawnEvent(ev) if ev.instrumented
            )
        })
        .count();
    let uninstrumented_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Dial9Event::TaskSpawnEvent(ev) if !ev.instrumented
            )
        })
        .count();

    assert_eq!(
        instrumented_count, INSTRUMENTED,
        "expected {INSTRUMENTED} instrumented spawns, got {instrumented_count}"
    );
    assert_eq!(
        uninstrumented_count, RAW,
        "expected {RAW} uninstrumented spawns, got {uninstrumented_count}"
    );

    // Verify spawn locations resolve and point to this test file.
    for event in &events {
        if let Dial9Event::TaskSpawnEvent(ev) = event
            && !ev.instrumented
        {
            assert!(
                ev.spawn_loc.contains("end_to_end.rs"),
                "uninstrumented spawn should point to this test file, got: {}",
                ev.spawn_loc
            );
        }
    }
}
