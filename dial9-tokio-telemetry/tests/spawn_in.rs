//! Focused tests for [`spawn_in`] and [`block_on`]: both must produce
//! instrumented tasks (spawn events marked instrumented, wake events recorded)
//! even though the future is handed over from a non-runtime thread.

mod common;

use common::{capture_processor, decode_all, small_mem_writer};
use dial9_tokio_telemetry::telemetry::{
    RecorderPipelineExt, TaskId, TokioAttachOptions, block_on, recorder, spawn_in,
};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

fn build_capturing_recorder() -> (
    dial9_core::recording::Recorder,
    tokio::runtime::Runtime,
    Arc<Mutex<Vec<Vec<u8>>>>,
) {
    let (capture, batches) = capture_processor();
    let rec = recorder(small_mem_writer())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(
        &rec,
        2,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );
    (rec, rt, batches)
}

/// `spawn_in` from a thread that belongs to no runtime still yields an
/// instrumented task: the spawn is marked instrumented and the task's wakes
/// are recorded.
#[test]
fn spawn_in_from_off_runtime_thread_is_instrumented() {
    let (rec, rt, batches) = build_capturing_recorder();

    let task_id: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));
    let id_w = task_id.clone();

    // The test thread is not a runtime thread; spawn_in targets the runtime.
    let join = spawn_in(rt.handle(), async move {
        *id_w.lock().unwrap() = tokio::task::try_id().map(TaskId::from);
        tokio::task::yield_now().await;
    });
    rt.block_on(join).unwrap();

    let id = task_id
        .lock()
        .unwrap()
        .expect("task observed its id")
        .to_u64();

    drop(rt);
    rec.graceful_shutdown(Duration::from_secs(5));

    let segments = batches.lock().unwrap();
    let events: Vec<SpawnEvent> = decode_all(&segments);
    let instrumented = events.iter().any(|e| {
        matches!(e, SpawnEvent::TaskSpawnEvent { task_id, instrumented: true, .. } if *task_id == id)
    });
    assert!(
        instrumented,
        "spawn_in task should be spawn-tracked as instrumented"
    );
    let woken = events.iter().any(
        |e| matches!(e, SpawnEvent::WakeEventEvent { woken_task_id, .. } if *woken_task_id == id),
    );
    assert!(woken, "spawn_in task should have wake events recorded");
}

/// `block_on` runs the root future as a spawned, instrumented task and
/// returns its output.
#[test]
fn block_on_instruments_the_root_future() {
    let (rec, rt, batches) = build_capturing_recorder();

    let task_id: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));
    let id_w = task_id.clone();

    let out = block_on(&rt, async move {
        *id_w.lock().unwrap() = tokio::task::try_id().map(TaskId::from);
        tokio::task::yield_now().await;
        7u32
    });
    assert_eq!(out, 7);

    let id = task_id
        .lock()
        .unwrap()
        .expect("root future ran as a task")
        .to_u64();

    drop(rt);
    rec.graceful_shutdown(Duration::from_secs(5));

    let segments = batches.lock().unwrap();
    let events: Vec<SpawnEvent> = decode_all(&segments);
    let instrumented = events.iter().any(|e| {
        matches!(e, SpawnEvent::TaskSpawnEvent { task_id, instrumented: true, .. } if *task_id == id)
    });
    assert!(
        instrumented,
        "block_on root future should be spawn-tracked as instrumented"
    );
}

/// `block_on` resumes the future's panic on the calling thread.
#[test]
fn block_on_propagates_panics() {
    let (rec, rt, _batches) = build_capturing_recorder();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(&rt, async {
            panic!("boom");
        })
    }));
    let payload = result.expect_err("panic must propagate");
    let msg = payload.downcast_ref::<&str>().copied().unwrap_or_default();
    assert_eq!(msg, "boom");

    drop(rt);
    rec.graceful_shutdown(Duration::from_secs(5));
}
