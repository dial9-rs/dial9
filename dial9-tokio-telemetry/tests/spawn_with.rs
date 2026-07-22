//! Integration tests for the custom-spawn tracing API:
//! - [`Dial9TokioHandle::spawn_with`]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all, decode_file};
use dial9_core::recording::Recorder;
use dial9_tokio_telemetry::telemetry::{
    Dial9TokioHandle, DiskBuffer, MemoryBuffer, RecorderPipelineExt, RecorderTokioExt, TaskId,
    TokioAttachOptions, recorder,
};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinSet;

#[derive(Debug, Deserialize)]
#[allow(dead_code, clippy::enum_variant_names)]
#[serde(tag = "event")]
enum SpawnEvent {
    TaskSpawnEvent {
        task_id: u64,
        spawn_loc: String,
        instrumented: bool,
    },
    WakeEventEvent {
        waker_task_id: u64,
        woken_task_id: u64,
    },
    #[serde(other)]
    Other,
}

/// Standard 2-worker multi_thread runtime with task tracking enabled.
fn build_capturing_runtime() -> (Recorder, tokio::runtime::Runtime, Arc<Mutex<Vec<Vec<u8>>>>) {
    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder()
                .task_tracking_enabled(true)
                .build(),
            |t| {
                t.enable_all();
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");
    (recorder, rt, batches)
}

/// `spawn_with(fut, |f| set.spawn(f))` produces `WakeEvent`s for the
/// spawned task — the same as `handle.spawn(fut)` would.
#[test]
fn spawn_with_joinset_emits_wake_events() {
    let (recorder, rt, batches) = build_capturing_runtime();

    let handle = Dial9TokioHandle::current();
    let spawned_id: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));
    let id_w = spawned_id.clone();

    rt.block_on(async move {
        let mut set: JoinSet<()> = JoinSet::new();
        handle.spawn_with(
            async move {
                *id_w.lock().unwrap() = tokio::task::try_id().map(TaskId::from);
                tokio::task::yield_now().await;
            },
            |f| set.spawn(f),
        );
        while set.join_next().await.is_some() {}
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let b = batches.lock().unwrap();
    let events: Vec<SpawnEvent> = decode_all(&b);
    let expected = spawned_id.lock().unwrap().expect("task id captured");
    let saw_wake = events.iter().any(|e| {
        matches!(e, SpawnEvent::WakeEventEvent { woken_task_id, .. } if *woken_task_id == expected.to_u64())
    });
    assert!(saw_wake, "expected WakeEvent for joinset task {expected:?}");
}

/// `spawn_with` flips the `TaskSpawn` `instrumented` flag for the spawn
/// performed inside the closure, AND because `JoinSet::spawn` is called
/// from that closure, its `#[track_caller]` resolves `spawn_loc` to the
/// closure call site (NOT the library).
#[test]
fn spawn_with_marks_taskspawn_and_preserves_caller() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("trace.bin");
    let writer = DiskBuffer::single_file(&trace_path).unwrap();
    let recorder = recorder(writer).build();
    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder()
                .task_tracking_enabled(true)
                .build(),
            |t| {
                t.enable_all();
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");

    let handle = Dial9TokioHandle::current();

    rt.block_on(async move {
        let mut set: JoinSet<()> = JoinSet::new();

        // Inside `spawn_with`: marked instrumented, caller = this file.
        handle.spawn_with(async {}, |f| set.spawn(f));

        // Outside `spawn_with`: NOT instrumented.
        tokio::spawn(async {}).await.unwrap();

        while set.join_next().await.is_some() {}
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    drop(rt);
    drop(recorder);

    let sealed = dir.path().join("trace.0.bin");
    let events: Vec<SpawnEvent> = decode_file(&sealed);

    let mut instrumented_user_loc = 0;
    let mut raw = 0;
    for event in &events {
        if let SpawnEvent::TaskSpawnEvent {
            spawn_loc,
            instrumented,
            ..
        } = event
        {
            if *instrumented {
                assert!(
                    spawn_loc.contains("spawn_with.rs"),
                    "instrumented spawn caller should resolve to the closure call site, got {spawn_loc}"
                );
                instrumented_user_loc += 1;
            } else {
                raw += 1;
            }
        }
    }
    assert_eq!(
        instrumented_user_loc, 1,
        "expected 1 instrumented TaskSpawn pointing to the closure call site"
    );
    assert!(raw >= 1, "expected at least 1 raw TaskSpawn, got {raw}");
}

/// `spawn_with` returns whatever the closure returns.
#[test]
fn spawn_with_returns_closure_value() {
    let recorder = recorder(common::small_mem_writer()).build();
    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder()
                .task_tracking_enabled(true)
                .build(),
            |t| {
                t.enable_all();
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");

    let handle = Dial9TokioHandle::current();

    rt.block_on(async move {
        let join = handle.spawn_with(async { 42u32 }, tokio::spawn);
        let value = join.await.unwrap();
        assert_eq!(value, 42);
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// `Dial9TokioHandle::spawn_with` composes with `JoinSet::spawn_on`
/// to target a specific runtime.
#[test]
fn runtime_handle_spawn_with_targets_correct_runtime() {
    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();

    let (recorder, rt_a) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder()
                .runtime_name("a")
                .task_tracking_enabled(true)
                .build(),
            |t| {
                t.worker_threads(1).enable_all().thread_name("rt-a");
            },
        )
        .expect("attach runtime a");

    let (recorder, rt_b) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder()
                .runtime_name("b")
                .task_tracking_enabled(true)
                .build(),
            |t| {
                t.worker_threads(1).enable_all().thread_name("rt-b");
            },
        )
        .expect("attach runtime b");

    // Both handles spawn through the same recorder; `spawn_with` runs the spawn
    // in the closure, so each task lands on the runtime its closure targets.
    let handle_a = Dial9TokioHandle::current();
    let handle_b = Dial9TokioHandle::current();

    let task_id_a: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));
    let task_id_b: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));

    let mut set_a: JoinSet<String> = JoinSet::new();
    let id_a = task_id_a.clone();
    handle_a.spawn_with(
        async move {
            *id_a.lock().unwrap() = tokio::task::try_id().map(TaskId::from);
            tokio::task::yield_now().await;
            std::thread::current().name().unwrap_or("?").to_string()
        },
        |f| set_a.spawn_on(f, rt_a.handle()),
    );

    let mut set_b: JoinSet<String> = JoinSet::new();
    let id_b = task_id_b.clone();
    handle_b.spawn_with(
        async move {
            *id_b.lock().unwrap() = tokio::task::try_id().map(TaskId::from);
            tokio::task::yield_now().await;
            std::thread::current().name().unwrap_or("?").to_string()
        },
        |f| set_b.spawn_on(f, rt_b.handle()),
    );

    let name_a = rt_a.block_on(async move { set_a.join_next().await.unwrap().unwrap() });
    let name_b = rt_b.block_on(async move { set_b.join_next().await.unwrap().unwrap() });

    assert!(name_a.starts_with("rt-a"), "got {name_a}");
    assert!(name_b.starts_with("rt-b"), "got {name_b}");

    drop(rt_a);
    drop(rt_b);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let task_id_a = task_id_a.lock().unwrap().expect("task id a captured");
    let task_id_b = task_id_b.lock().unwrap().expect("task id b captured");
    let b = batches.lock().unwrap();
    let events: Vec<SpawnEvent> = decode_all(&b);

    for expected in [task_id_a, task_id_b] {
        let saw_instrumented_spawn = events.iter().any(|event| {
            matches!(
                event,
                SpawnEvent::TaskSpawnEvent {
                    task_id,
                    instrumented: true,
                    ..
                } if *task_id == expected.to_u64()
            )
        });
        assert!(
            saw_instrumented_spawn,
            "expected instrumented TaskSpawn for runtime handle task {expected:?}"
        );

        let saw_wake = events.iter().any(|event| {
            matches!(event, SpawnEvent::WakeEventEvent { woken_task_id, .. } if *woken_task_id == expected.to_u64())
        });
        assert!(
            saw_wake,
            "expected WakeEvent for runtime handle task {expected:?}"
        );
    }
}
