#![cfg(feature = "taskdump")]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{
    Dial9TokioHandle, MemoryBuffer, RecorderPipelineExt, TaskDumpConfig, TaskId,
    TokioAttachOptions, recorder,
};
use std::time::Duration;

async fn pending_task(handle: &Dial9TokioHandle) -> u64 {
    let task = handle.spawn(async {
        tokio::time::sleep(Duration::from_millis(20)).await;
    });
    let id = TaskId::from(task.id()).to_u64();
    task.await.unwrap();
    id
}

async fn calibrate(handle: &Dial9TokioHandle) {
    handle
        .spawn(async {
            tokio::time::sleep(Duration::from_millis(1050)).await;
        })
        .await
        .unwrap();
}

fn check_worker_capture(events: Vec<Dial9Event>, before: u64, after: u64) {
    let worker = |id| {
        events
            .iter()
            .find_map(|e| match e {
                Dial9Event::PollStartEvent(p) if p.task_id == id => Some(p.worker_id.as_u64()),
                _ => None,
            })
            .unwrap()
    };
    let dumps = |id| {
        events
            .iter()
            .filter(|e| {
                matches!(e,
                    Dial9Event::TaskDumpEvent(d) if d.task_id == id
                )
            })
            .count()
    };
    assert_eq!(worker(before), worker(after));
    assert!(
        dumps(before) > 0,
        "control task must capture after calibration"
    );
    assert!(
        dumps(after) > 0,
        "worker {} must retain its calibrated sampler on another thread; before captures={}",
        worker(before),
        dumps(before)
    );
}

#[test]
fn block_in_place_handoff_preserves_sampler() {
    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(
        &recorder,
        1,
        TokioAttachOptions::builder()
            .task_dump_config(
                TaskDumpConfig::builder()
                    .captures_per_second_per_worker(1000)
                    .build(),
            )
            .build(),
    );
    let handle = Dial9TokioHandle::current();
    let (before, after) = rt.block_on(async {
        calibrate(&handle).await;
        let before = pending_task(&handle).await;
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let blocker = handle.spawn(async move {
            tokio::task::block_in_place(|| {
                started_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            });
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let after = pending_task(&handle).await;
        resume_tx.send(()).unwrap();
        blocker.await.unwrap();
        (before, after)
    });
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));
    check_worker_capture(decode_all(&batches.lock().unwrap()), before, after);
}

#[test]
fn current_thread_driver_migration_preserves_sampler() {
    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_dump_config(
                TaskDumpConfig::builder()
                    .captures_per_second_per_worker(1000)
                    .build(),
            )
            .build(),
    );
    let handle = Dial9TokioHandle::current();
    let first_handle = handle.clone();
    let (rt, before) = std::thread::spawn(move || {
        let before = rt.block_on(async {
            calibrate(&first_handle).await;
            pending_task(&first_handle).await
        });
        (rt, before)
    })
    .join()
    .unwrap();
    let after = std::thread::spawn(move || rt.block_on(pending_task(&handle)))
        .join()
        .unwrap();
    recorder.graceful_shutdown(Duration::from_secs(1));
    check_worker_capture(decode_all(&batches.lock().unwrap()), before, after);
}

#[test]
fn original_and_replacement_threads_can_sample_concurrently() {
    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(
        &recorder,
        1,
        TokioAttachOptions::builder()
            .task_dump_config(TaskDumpConfig::default())
            .build(),
    );
    let handle = Dial9TokioHandle::current();
    let (first, second) = rt.block_on(async {
        calibrate(&handle).await;
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let original_barrier = barrier.clone();
        let first = handle.spawn(async move {
            tokio::task::block_in_place(|| {
                started_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            original_barrier.wait();
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = handle.spawn(async move {
            resume_tx.send(()).unwrap();
            // Both threads finish their normal polls on the same logical worker.
            barrier.wait();
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        let ids = (
            TaskId::from(first.id()).to_u64(),
            TaskId::from(second.id()).to_u64(),
        );
        first.await.unwrap();
        second.await.unwrap();
        ids
    });
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));
    check_worker_capture(decode_all(&batches.lock().unwrap()), first, second);
}

#[test]
fn long_poll_uses_pending_time_for_calibration() {
    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_dump_config(TaskDumpConfig::default())
            .build(),
    );
    let handle = Dial9TokioHandle::current();
    rt.block_on(async {
        handle
            .spawn(async {
                std::thread::sleep(Duration::from_millis(1050));
                tokio::time::sleep(Duration::from_millis(20)).await;
            })
            .await
            .unwrap();
    });
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));
    let events: Vec<Dial9Event> = decode_all(&batches.lock().unwrap());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Dial9Event::TaskDumpEvent(_))),
        "the first pending transition is already past calibration, although its poll began earlier"
    );
}

#[test]
fn nested_runtime_does_not_replace_the_enclosing_polls_sampler() {
    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let nested = common::attach_current_thread(&recorder, TokioAttachOptions::default());
    let rt = common::attach(
        &recorder,
        1,
        TokioAttachOptions::builder()
            .task_dump_config(TaskDumpConfig::default())
            .build(),
    );
    let handle = Dial9TokioHandle::current();
    let task_id = rt.block_on(async {
        calibrate(&handle).await;
        let nested_handle = handle.clone();
        let task = handle.spawn(async move {
            tokio::task::block_in_place(|| {
                nested.block_on(async {
                    nested_handle
                        .spawn(async { tokio::task::yield_now().await })
                        .await
                        .unwrap();
                });
                drop(nested);
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        let id = TaskId::from(task.id()).to_u64();
        task.await.unwrap();
        id
    });
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));
    let events: Vec<Dial9Event> = decode_all(&batches.lock().unwrap());
    assert!(
        events.iter().any(|event| {
            matches!(event, Dial9Event::TaskDumpEvent(dump) if dump.task_id == task_id)
        }),
        "the enclosing poll must use its own worker's calibrated sampler"
    );
}
