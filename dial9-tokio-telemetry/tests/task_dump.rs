#![cfg(feature = "taskdump")]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::{InMemoryWriter, TaskDumpConfig, TracedRuntime};
use serde::Deserialize;
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

/// A task that stays idle longer than the threshold between polls should
/// produce at least one `TaskDump` event.
#[test]
fn task_dump_emitted_for_long_sleep() {
    let (capture, batches) = capture_processor();

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_task_dumps(TaskDumpConfig::builder().rng_seed(42).build())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    let handle = guard.tokio_handle(runtime.handle());
    runtime.block_on(async {
        let join = handle.spawn(async {
            // Well above the 10ms default threshold.
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        join.await.unwrap();
    });

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

    let b = batches.lock().unwrap();
    let events: Vec<DumpEvent> = decode_all(&b);
    let dumps: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, DumpEvent::TaskDumpEvent { .. }))
        .collect();

    assert!(!dumps.is_empty(), "expected TaskDump events");
    for dump in &dumps {
        if let DumpEvent::TaskDumpEvent { callchain } = dump {
            assert!(!callchain.is_empty(), "callchain must be non-empty");
        }
    }
}

/// A task whose idles are all below threshold should produce zero dumps.
#[test]
fn no_task_dump_for_short_sleep() {
    let (capture, batches) = capture_processor();

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_task_dumps(
            TaskDumpConfig::builder()
                .idle_threshold(Duration::from_secs(1))
                .rng_seed(42)
                .build(),
        )
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    let handle = guard.tokio_handle(runtime.handle());
    runtime.block_on(async {
        let join = handle.spawn(async {
            tokio::time::sleep(Duration::from_millis(1)).await;
        });
        join.await.unwrap();
    });

    drop(runtime);
    guard
        .graceful_shutdown(Duration::from_secs(1))
        .expect("clean shutdown");

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

        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();
        let mut tb = TracedRuntime::builder().with_task_tracking(true);
        if enable {
            tb = tb.with_task_dumps(TaskDumpConfig::builder().rng_seed(42).build());
        }
        let (runtime, guard) = tb
            .with_custom_pipeline(|p| p.pipe(capture))
            .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
            .unwrap();

        let handle = guard.tokio_handle(runtime.handle());
        runtime.block_on(async {
            let join = handle.spawn(async {
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
            });
            join.await.unwrap();
        });
        drop(runtime);
        guard
            .graceful_shutdown(Duration::from_secs(1))
            .expect("clean shutdown");

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

    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    let (runtime, guard) = TracedRuntime::builder()
        .with_task_tracking(true)
        .with_task_dumps(TaskDumpConfig::builder().rng_seed(42).build())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build_and_start(builder, InMemoryWriter::new(CAPTURE_BUFFER_SIZE).unwrap())
        .unwrap();

    let handle = guard.tokio_handle(runtime.handle());
    runtime.block_on(async {
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

    drop(runtime);
    guard
        .graceful_shutdown(std::time::Duration::from_secs(1))
        .expect("clean shutdown");

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
