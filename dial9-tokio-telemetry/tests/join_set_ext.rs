mod common;

#[path = "support/join_set_helper.rs"]
mod join_set_helper;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_core::recording::Recorder;
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{
    MemoryBuffer, RecorderPipelineExt, TaskId, TokioAttachOptions, recorder,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinSet;

fn build_runtime() -> (Recorder, tokio::runtime::Runtime, Arc<Mutex<Vec<Vec<u8>>>>) {
    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|pipeline| pipeline.pipe(capture))
        .build();
    let runtime = common::attach(
        &recorder,
        2,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );
    (recorder, runtime, batches)
}

async fn yielding_task() -> TaskId {
    let task_id = tokio::task::try_id().map(TaskId::from).unwrap();
    tokio::task::yield_now().await;
    task_id
}

fn assert_instrumented_spawn_at(batches: &Mutex<Vec<Vec<u8>>>, task_id: TaskId, call_line: u32) {
    let events: Vec<Dial9Event> = decode_all(&batches.lock().unwrap());
    let expected_loc = format!("join_set_ext.rs:{call_line}:");

    // With tokio's task hooks the caller lands on the TaskSpawn event; without
    // them it lands on the traced wrapper's PollStart.
    #[cfg(tokio_unstable)]
    assert!(events.iter().any(|event| {
        matches!(
            event,
            Dial9Event::TaskSpawnEvent(event)
                if event.task_id == task_id.to_u64()
                    && event.instrumented
                    && event.spawn_loc.contains(&expected_loc)
        )
    }));
    #[cfg(not(tokio_unstable))]
    assert!(events.iter().any(|event| {
        matches!(
            event,
            Dial9Event::PollStartEvent(event)
                if event.task_id == task_id.to_u64()
                    && event.spawn_loc.contains(&expected_loc)
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            Dial9Event::WakeEvent(event) if event.woken_task_id == task_id.to_u64()
        )
    }));
}

#[test]
fn join_set_ext_preserves_caller_through_helper() {
    let (recorder, runtime, batches) = build_runtime();

    let (task_id, call_line) = runtime.block_on(async move {
        let mut set = JoinSet::new();
        let call_line = line!() + 1;
        join_set_helper::spawn_traced(&mut set, yielding_task());
        let task_id = set.join_next().await.unwrap().unwrap();
        (task_id, call_line)
    });

    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));

    assert_instrumented_spawn_at(&batches, task_id, call_line);
}

#[test]
fn join_set_ext_on_preserves_caller_through_helper() {
    let (recorder, runtime, batches) = build_runtime();
    let mut set = JoinSet::new();

    let call_line = line!() + 1;
    join_set_helper::spawn_traced_on(&mut set, yielding_task(), runtime.handle());
    let task_id = runtime.block_on(async { set.join_next().await.unwrap().unwrap() });

    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));

    assert_instrumented_spawn_at(&batches, task_id, call_line);
}
