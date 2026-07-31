//! Tests that `QueueSampleEvent.active_tasks` reflects the number of alive tasks
//! via `RuntimeMetrics::num_alive_tasks()`.

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{
    MemoryBuffer, RecorderPipelineExt, TokioAttachOptions, recorder,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;

/// Spawn a known number of long-lived tasks, verify that `QueueSampleEvent`
/// reports at least that many active via `RuntimeMetrics::num_alive_tasks()`.
#[test]
fn active_tasks_reflects_spawned_tasks_via_runtime_metrics() {
    let (capture, batches) = capture_processor();
    let num_tasks: usize = 20;

    let rec = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(
        &rec,
        2,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );

    rt.block_on(async {
        let barrier = Arc::new(Barrier::new(num_tasks + 1));
        let mut handles = Vec::new();
        for _ in 0..num_tasks {
            let b = barrier.clone();
            handles.push(tokio::spawn(async move {
                b.wait().await;
                // Stay alive long enough for the flush source to sample.
                tokio::time::sleep(Duration::from_millis(200)).await;
            }));
        }

        // All tasks alive — release them.
        barrier.wait().await;
        // TokioRuntimesSource samples every 10ms; give it time to emit.
        tokio::time::sleep(Duration::from_millis(100)).await;

        for h in handles {
            h.await.unwrap();
        }
    });

    drop(rt);
    rec.graceful_shutdown(Duration::from_secs(1));

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);

    let max_active = events
        .iter()
        .filter_map(|e| match e {
            Dial9Event::QueueSampleEvent(ev) => ev.active_tasks,
            _ => None,
        })
        .max()
        .expect("expected at least one QueueSampleEvent with active_tasks");

    assert!(
        max_active >= num_tasks as u64,
        "expected at least {num_tasks} active tasks in some QueueSampleEvent, \
         but max observed was {max_active}"
    );
}

/// After all spawned tasks complete, `active_tasks` should drop back down to
/// near zero — only runtime-internal tasks (if any) remain.
#[test]
fn active_tasks_decreases_after_tasks_complete() {
    let (capture, batches) = capture_processor();
    let num_tasks: usize = 20;

    let rec = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(
        &rec,
        2,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );

    rt.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..num_tasks {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Give TokioRuntimesSource (10ms interval) time to sample after tasks complete.
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    drop(rt);
    rec.graceful_shutdown(Duration::from_secs(1));

    let b = batches.lock().unwrap();
    let events: Vec<Dial9Event> = decode_all(&b);

    let active_tasks_values: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Dial9Event::QueueSampleEvent(ev) => ev.active_tasks,
            _ => None,
        })
        .collect();

    assert!(
        !active_tasks_values.is_empty(),
        "expected at least one QueueSampleEvent with active_tasks"
    );

    // After all spawned tasks have completed and the runtime is dropped,
    // the final sample should show near-zero active tasks.
    let last_active = *active_tasks_values.last().unwrap();
    assert!(
        last_active <= 3,
        "expected active_tasks near zero after all tasks completed, \
         got {last_active} (spawned {num_tasks})"
    );
}
