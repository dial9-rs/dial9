mod common;

#[path = "support/join_set_helper.rs"]
mod join_set_helper;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_core::recording::Recorder;
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{
    MemoryBuffer, RecorderPipelineExt, TokioAttachOptions, recorder,
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

#[test]
fn join_set_ext_preserves_caller_through_helper() {
    let (recorder, runtime, batches) = build_runtime();

    let call_line = runtime.block_on(async move {
        let mut set = JoinSet::new();
        let call_line = line!() + 1;
        join_set_helper::spawn_traced(&mut set, async {});
        set.join_next().await.unwrap().unwrap();
        call_line
    });

    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let events: Vec<Dial9Event> = decode_all(&batches.lock().unwrap());
    let expected_loc = format!("join_set_ext.rs:{call_line}:");

    assert!(events.iter().any(|event| {
        matches!(
            event,
            Dial9Event::TaskSpawnEvent(event)
                if event.instrumented && event.spawn_loc.contains(&expected_loc)
        )
    }));
}
