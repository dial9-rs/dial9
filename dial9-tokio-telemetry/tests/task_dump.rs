#![cfg(feature = "taskdump")]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::{
    Dial9TokioHandle, MemoryBuffer, RecorderPipelineExt, TaskDumpConfig, TokioAttachOptions,
    recorder,
};
use serde::Deserialize;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::task::JoinSet;

#[derive(Debug, Deserialize)]
#[allow(dead_code, clippy::enum_variant_names)]
#[serde(tag = "event")]
enum DumpEvent {
    TaskDumpEvent {
        callchain: Vec<u64>,
    },
    PollStartEvent {
        timestamp_ns: u64,
    },
    PollEndEvent {
        timestamp_ns: u64,
    },
    WakeEventEvent {
        timestamp_ns: u64,
    },
    #[serde(other)]
    Other,
}

fn task_dump_callchains(spawn_with_dial9: bool) -> Vec<Vec<u64>> {
    let (capture, batches) = capture_processor();

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .maybe_task_dump_config(Some(TaskDumpConfig::builder().rng_seed(42).build()))
            .build(),
    );

    let handle = Dial9TokioHandle::current();
    rt.block_on(async {
        let future = async {
            // Well above the 10ms default threshold.
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let join = if spawn_with_dial9 {
            handle.spawn(future)
        } else {
            tokio::spawn(future)
        };
        join.await.unwrap();
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let b = batches.lock().unwrap();
    let events: Vec<DumpEvent> = decode_all(&b);
    events
        .into_iter()
        .filter_map(|event| match event {
            DumpEvent::TaskDumpEvent { callchain } => Some(callchain),
            _ => None,
        })
        .collect()
}

/// A task that stays idle longer than the threshold between polls should
/// produce at least one `TaskDump` event.
#[test]
fn task_dump_emitted_for_long_sleep() {
    let dial9_callchains = task_dump_callchains(true);
    assert!(
        !dial9_callchains.is_empty(),
        "expected TaskDump events from the Dial9-spawned task"
    );
    for callchain in dial9_callchains {
        assert!(!callchain.is_empty(), "callchain must be non-empty");
    }
}

/// A task spawned directly through Tokio should not produce task dumps.
#[test]
fn tokio_spawn_does_not_emit_task_dump() {
    let tokio_callchains = task_dump_callchains(false);
    assert!(
        tokio_callchains.is_empty(),
        "a task spawned directly with tokio::spawn must not produce TaskDump events"
    );
}

/// A task whose idles are all below threshold should produce zero dumps.
#[test]
fn no_task_dump_for_short_sleep() {
    let (capture, batches) = capture_processor();

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .maybe_task_dump_config(Some(
                TaskDumpConfig::builder()
                    .idle_threshold(Duration::from_secs(1))
                    .rng_seed(42)
                    .build(),
            ))
            .build(),
    );

    let handle = Dial9TokioHandle::current();
    rt.block_on(async {
        let join = handle.spawn(async {
            tokio::time::sleep(Duration::from_millis(1)).await;
        });
        join.await.unwrap();
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let b = batches.lock().unwrap();
    let events: Vec<DumpEvent> = decode_all(&b);
    let dump_count = events
        .iter()
        .filter(|e| matches!(e, DumpEvent::TaskDumpEvent { .. }))
        .count();
    assert_eq!(dump_count, 0, "expected no TaskDump events");
}

/// Wrapping with `TaskDumped` must not produce duplicate wake or poll events.
#[test]
fn task_dump_does_not_produce_extra_events() {
    fn run(enable: bool) -> (usize, usize, usize) {
        let (capture, batches) = capture_processor();

        let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
            .with_custom_pipeline(|p| p.pipe(capture))
            .build();
        let rt = common::attach_current_thread(
            &recorder,
            TokioAttachOptions::builder()
                .task_tracking_enabled(true)
                .maybe_task_dump_config(
                    enable.then(|| TaskDumpConfig::builder().rng_seed(42).build()),
                )
                .build(),
        );

        let handle = Dial9TokioHandle::current();
        rt.block_on(async {
            let join = handle.spawn(async {
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
            });
            join.await.unwrap();
        });
        drop(rt);
        recorder.graceful_shutdown(Duration::from_secs(1));

        let b = batches.lock().unwrap();
        let events: Vec<DumpEvent> = decode_all(&b);
        let mut starts = 0usize;
        let mut ends = 0usize;
        let mut wakes = 0usize;
        for e in &events {
            match e {
                DumpEvent::PollStartEvent { .. } => starts += 1,
                DumpEvent::PollEndEvent { .. } => ends += 1,
                DumpEvent::WakeEventEvent { .. } => wakes += 1,
                _ => {}
            }
        }
        (starts, ends, wakes)
    }

    let baseline = run(false);
    let with_dumps = run(true);
    assert_eq!(
        baseline, with_dumps,
        "enabling task dumps changed PollStart/PollEnd/WakeEvent counts: {baseline:?} vs {with_dumps:?}"
    );
}

/// Custom spawn APIs should get the same task-dump instrumentation.
#[test]
fn spawn_with_joinset_emits_task_dump() {
    let (capture, batches) = capture_processor();

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .maybe_task_dump_config(Some(TaskDumpConfig::builder().rng_seed(42).build()))
            .build(),
    );

    let handle = Dial9TokioHandle::current();
    rt.block_on(async {
        let mut set: JoinSet<()> = JoinSet::new();
        handle.spawn_with(
            async {
                // Well above the 10ms default threshold.
                tokio::time::sleep(Duration::from_millis(50)).await;
            },
            |f| set.spawn(f),
        );
        while set.join_next().await.is_some() {}
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let b = batches.lock().unwrap();
    let events: Vec<DumpEvent> = decode_all(&b);
    let dumps: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, DumpEvent::TaskDumpEvent { .. }))
        .collect();

    assert!(
        !dumps.is_empty(),
        "expected TaskDump events from spawn_with JoinSet task"
    );
}

/// A contract-abiding future that completes on its **second** poll, used to
/// reproduce the race condition in the regression test below.
struct CompletesOnSecondPoll {
    polls: u32,
}

impl Future for CompletesOnSecondPoll {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.polls += 1;
        match self.polls {
            // Park and arm the waker, as a future waiting on an external
            // resource does; this pending wake reschedules the task.
            1 => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            // The capture re-poll completes the future.
            2 => Poll::Ready(()),
            // Any further poll is a poll-after-`Ready` contract violation.
            n => panic!("future polled again after it returned Ready (poll #{n})"),
        }
    }
}

/// Regression: the task-dump capture re-poll must not complete a future and
/// then let it be polled again. Before the fix this panicked with a
/// poll-after-`Ready` (surfacing as a `JoinError`); the task must instead
/// complete cleanly.
#[test]
fn task_dump_capture_repoll_does_not_cause_poll_after_ready() {
    let (capture, _batches) = capture_processor();

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .maybe_task_dump_config(Some(TaskDumpConfig::builder().rng_seed(42).build()))
            .build(),
    );

    let handle = Dial9TokioHandle::current();
    let result = rt.block_on(async { handle.spawn(CompletesOnSecondPoll { polls: 0 }).await });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    assert!(
        result.is_ok(),
        "spawned future was polled after it returned Ready \
         (TaskDumped re-polled a completed future): {result:?}"
    );
}
