#![cfg(feature = "taskdump")]

mod common;

use common::{CAPTURE_BUFFER_SIZE, capture_processor, decode_all};
use dial9_tokio_telemetry::telemetry::{
    Dial9TokioHandle, MemoryBuffer, RecorderPipelineExt, TaskSamplingConfig, TokioAttachOptions,
    recorder,
};
use serde::Deserialize;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::task::JoinSet;

#[derive(Debug, Deserialize)]
#[allow(dead_code, clippy::enum_variant_names)]
#[serde(tag = "event")]
enum DumpEvent {
    TaskSampleEvent {
        callchain: Vec<u64>,
        timestamp_ns: u64,
        task_id: u64,
        inclusion_probability: f64,
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

// Drive a worker during calibration before exercising the selected path.
async fn warm_up(handle: &Dial9TokioHandle) {
    handle
        .spawn(async { tokio::time::sleep(Duration::from_millis(1050)).await })
        .await
        .unwrap();
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
            .maybe_task_sampling_config(Some(TaskSamplingConfig::builder().rng_seed(42).build()))
            .build(),
    );

    let handle = Dial9TokioHandle::current();
    rt.block_on(async {
        warm_up(&handle).await;
        let future = async {
            // A pending Tokio leaf supplies a capture callchain.
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
            DumpEvent::TaskSampleEvent { callchain, .. } => Some(callchain),
            _ => None,
        })
        .collect()
}

/// A selected pending transition after calibration should
/// produce at least one `TaskDump` event.
#[test]
fn task_dump_emitted_after_calibration() {
    let dial9_callchains = task_dump_callchains(true);
    assert!(
        !dial9_callchains.is_empty(),
        "expected TaskDump events from the Dial9-spawned task"
    );
    for callchain in dial9_callchains {
        assert!(!callchain.is_empty(), "callchain must be non-empty");
    }
}

/// Skipping one capture must not suppress captures for the rest of the task.
#[test]
fn task_dump_resumes_capture_after_skip() {
    let (capture, batches) = capture_processor();

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .maybe_task_sampling_config(Some(
                TaskSamplingConfig::builder()
                    .captures_per_second_per_worker(1_000_000)
                    .rng_seed(42)
                    .build(),
            ))
            .build(),
    );

    let handle = Dial9TokioHandle::current();
    rt.block_on(async {
        warm_up(&handle).await;
        handle
            .spawn(async {
                for _ in 0..3 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    let b = batches.lock().unwrap();
    let events: Vec<DumpEvent> = decode_all(&b);
    let dump_count = events
        .iter()
        .filter(|e| matches!(e, DumpEvent::TaskSampleEvent { .. }))
        .count();
    // Without capture-induced wakes, the second idle is skipped. With them,
    // the extra polls consume the skip and all three idles can be captured.
    assert!(
        (2..=3).contains(&dump_count),
        "capture must resume after a skipped poll; got {dump_count} dumps"
    );
}

fn assert_idle_task_does_not_spin(multi_thread: bool) {
    let (capture, _batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let options = TokioAttachOptions::builder()
        .task_tracking_enabled(true)
        .maybe_task_sampling_config(Some(TaskSamplingConfig::builder().rng_seed(42).build()))
        .build();
    let rt = if multi_thread {
        common::attach(&recorder, 2, options)
    } else {
        common::attach_current_thread(&recorder, options)
    };

    let handle = Dial9TokioHandle::current();
    let result = rt.block_on(async {
        warm_up(&handle).await;
        handle
            .spawn(async {
                let sleep = tokio::time::sleep(Duration::from_millis(50));
                tokio::pin!(sleep);
                let mut polls = 0;
                poll_fn(|cx| {
                    polls += 1;
                    // Includes capture re-polls and allows a few spurious
                    // wakes, but fails promptly on a wake-and-capture loop.
                    assert!(polls <= 10, "task-dump capture keeps polling an idle task");
                    sleep.as_mut().poll(cx)
                })
                .await;
            })
            .await
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));
    result.expect("idle task must complete without a capture loop");
}

#[test]
fn task_dump_does_not_spin_current_thread() {
    assert_idle_task_does_not_spin(false);
}

#[test]
fn task_dump_does_not_spin_multi_thread() {
    assert_idle_task_does_not_spin(true);
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

/// Calibration collects transitions without capturing stacks.
#[test]
fn no_task_dump_during_calibration() {
    let (capture, batches) = capture_processor();

    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .maybe_task_sampling_config(Some(
                TaskSamplingConfig::builder()
                    .captures_per_second_per_worker(1)
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
        .filter(|e| matches!(e, DumpEvent::TaskSampleEvent { .. }))
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
                .maybe_task_sampling_config(
                    enable.then(|| TaskSamplingConfig::builder().rng_seed(42).build()),
                )
                .build(),
        );

        let handle = Dial9TokioHandle::current();
        rt.block_on(async {
            warm_up(&handle).await;
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
            .maybe_task_sampling_config(Some(TaskSamplingConfig::builder().rng_seed(42).build()))
            .build(),
    );

    let handle = Dial9TokioHandle::current();
    rt.block_on(async {
        warm_up(&handle).await;
        let mut set: JoinSet<()> = JoinSet::new();
        handle.spawn_with(
            async {
                // A pending Tokio leaf supplies a capture callchain.
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
        .filter(|e| matches!(e, DumpEvent::TaskSampleEvent { .. }))
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
            .maybe_task_sampling_config(Some(TaskSamplingConfig::builder().rng_seed(42).build()))
            .build(),
    );

    let handle = Dial9TokioHandle::current();
    let result = rt.block_on(async {
        warm_up(&handle).await;
        handle.spawn(CompletesOnSecondPoll { polls: 0 }).await
    });

    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));

    assert!(
        result.is_ok(),
        "spawned future was polled after it returned Ready \
         (TaskDumped re-polled a completed future): {result:?}"
    );
}

/// A task aborted while still waiting must already have emitted every leaf,
/// once, with the capture's probability and actual (post-work) timestamp.
#[test]
fn selected_capture_emits_all_leaves_before_any_later_poll() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_sampling_config(
                TaskSamplingConfig::builder()
                    .captures_per_second_per_worker(1000)
                    .rng_seed(42)
                    .build(),
            )
            .build(),
    );
    let handle = Dial9TokioHandle::current();
    let started = Arc::new(AtomicU64::new(0));
    rt.block_on(async {
        warm_up(&handle).await;
        let started = started.clone();
        let task = handle.spawn(async move {
            // Distinguish the capture timestamp from the cached poll start.
            std::thread::sleep(Duration::from_millis(10));
            started.store(dial9_core::clock::clock_monotonic_ns(), Ordering::Relaxed);
            let a = tokio::sync::Notify::new();
            let b = tokio::sync::Notify::new();
            tokio::select! { _ = a.notified() => {}, _ = b.notified() => {} }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    });
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));
    let events: Vec<DumpEvent> = decode_all(&batches.lock().unwrap());
    let dumps: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            DumpEvent::TaskSampleEvent {
                timestamp_ns,
                task_id,
                inclusion_probability,
                callchain,
            } => {
                assert!(!callchain.is_empty());
                assert_eq!(*inclusion_probability, 1.0);
                assert!(*timestamp_ns >= started.load(Ordering::Relaxed));
                Some((*timestamp_ns, *task_id))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        dumps.len(),
        2,
        "one selected capture must emit both waiting leaves"
    );
    assert_eq!(
        dumps[0], dumps[1],
        "sibling callchains must share their capture key"
    );
}

#[test]
fn worker_metadata_survives_runtime_switches_and_segment_rotation() {
    use dial9_tokio_telemetry::telemetry::DiskBuffer;
    use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    let (capture, batches) = capture_processor();
    let recorder = recorder(
        DiskBuffer::builder()
            .base_path(dir.path())
            .max_total_size(16 * 1024 * 1024)
            .max_file_size(64 * 1024)
            .rotation_period(Duration::from_millis(30))
            .build()
            .unwrap(),
    )
    .with_custom_pipeline(|p| p.pipe(capture))
    .build();
    let attach = |name: &str, rate: Option<u32>| {
        common::attach_current_thread(
            &recorder,
            TokioAttachOptions::builder()
                .runtime_name(name)
                .maybe_task_sampling_config(rate.map(|rate| {
                    TaskSamplingConfig::builder()
                        .captures_per_second_per_worker(rate)
                        .build()
                }))
                .build(),
        )
    };
    let a = attach("a", Some(10));
    let b = attach("b", Some(20));
    let disabled = attach("disabled", None);
    let handle = Dial9TokioHandle::current();
    a.block_on(warm_up(&handle));
    b.block_on(warm_up(&handle));
    disabled.block_on(async {
        handle
            .spawn(async { tokio::task::yield_now().await })
            .await
            .unwrap();
    });
    // Return to each already-calibrated worker after another runtime used TLS.
    for rt in [&a, &b] {
        rt.block_on(async {
            for _ in 0..3 {
                handle
                    .spawn(async { tokio::time::sleep(Duration::from_millis(50)).await })
                    .await
                    .unwrap();
                // Small workloads otherwise wait for the periodic TL drain.
                dial9_core::test_util::drain_thread_local(recorder.handle().shared().unwrap());
            }
        });
    }
    drop((a, b, disabled));
    recorder.graceful_shutdown(Duration::from_secs(1));

    let batches = batches.lock().unwrap();
    let segments: Vec<Vec<Dial9Event>> = batches
        .iter()
        .map(|b| decode_all(std::slice::from_ref(b)))
        .collect();
    let mut last_active = BTreeMap::new();
    let mut complete_segments = 0;
    let mut captured_tasks = std::collections::BTreeSet::new();
    for events in &segments {
        let mut metadata = BTreeMap::new();
        for event in events {
            match event {
                Dial9Event::SegmentMetadataEvent(m) => metadata.extend(m.entries.clone()),
                Dial9Event::TaskSampleEvent(d) => {
                    captured_tasks.insert(d.task_id);
                }
                _ => {}
            }
        }
        if !metadata.contains_key("runtime.disabled") {
            continue;
        }
        assert_eq!(metadata["task_sampling.sampler"], "per_worker_bernoulli_v1");
        let mut complete = true;
        for (runtime, rate) in [("a", "10"), ("b", "20")] {
            let worker = &metadata[&format!("runtime.{runtime}")];
            assert_eq!(
                metadata[&format!("task_sampling.worker.{worker}.captures_per_second")],
                rate
            );
            let key = format!("task_sampling.worker.{worker}.sampling_started_at_ns");
            if let Some(active) = metadata.get(&key) {
                assert!(active.parse::<u64>().unwrap() > 0);
                if let Some(previous) = last_active.insert(key, active.clone()) {
                    assert_eq!(&previous, active);
                }
            } else {
                complete = false;
            }
        }
        let disabled_worker = &metadata["runtime.disabled"];
        assert!(!metadata.contains_key(&format!(
            "task_sampling.worker.{disabled_worker}.captures_per_second"
        )));
        if complete {
            complete_segments += 1;
        }
    }
    assert!(
        complete_segments >= 2,
        "sampling metadata must persist across segment rotation: complete={complete_segments}, segments={}, active={last_active:?}",
        segments.len()
    );
    assert!(
        captured_tasks.len() >= 2,
        "returning to a worker must retain its calibrated sampler"
    );
}

#[test]
fn every_worker_and_task_participates_in_capture_sampling() {
    use dial9_tokio_telemetry::telemetry::TaskId;
    use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
    use std::collections::{BTreeSet, HashMap};
    use std::sync::{Arc, Barrier};

    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let rt = common::attach(
        &recorder,
        2,
        TokioAttachOptions::builder()
            .task_sampling_config(
                TaskSamplingConfig::builder()
                    .captures_per_second_per_worker(1000)
                    .build(),
            )
            .build(),
    );
    let handle = Dial9TokioHandle::current();
    let mut tasks = BTreeSet::new();
    rt.block_on(async {
        warm_up(&handle).await;
        for _ in 0..2 {
            let barrier = Arc::new(Barrier::new(2));
            let mut joins = Vec::new();
            for _ in 0..2 {
                let barrier = barrier.clone();
                let join = handle.spawn(async move {
                    // Occupy both workers so neither can service both tasks.
                    barrier.wait();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                });
                tasks.insert(TaskId::from(join.id()).to_u64());
                joins.push(join);
            }
            for join in joins {
                join.await.unwrap();
            }
        }
    });
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(1));
    let events: Vec<Dial9Event> = decode_all(&batches.lock().unwrap());
    let mut polls = HashMap::<u64, Vec<(u64, u64)>>::new();
    for event in &events {
        if let Dial9Event::PollStartEvent(poll) = event {
            polls
                .entry(poll.task_id)
                .or_default()
                .push((poll.timestamp_ns, poll.worker_id.as_u64()));
        }
    }
    let mut sampled_workers = BTreeSet::new();
    let mut sampled_tasks = BTreeSet::new();
    for event in events {
        if let Dial9Event::TaskSampleEvent(dump) = event {
            assert_eq!(dump.inclusion_probability, 1.0);
            let (_, worker) = polls[&dump.task_id]
                .iter()
                .filter(|(ts, _)| *ts <= dump.timestamp_ns)
                .max_by_key(|(ts, _)| *ts)
                .unwrap();
            sampled_workers.insert(*worker);
            sampled_tasks.insert(dump.task_id);
        }
    }
    assert_eq!(sampled_workers.len(), 2);
    assert_eq!(sampled_tasks, tasks);
}

#[test]
fn legacy_sampled_and_disabled_runtimes_keep_their_capture_mode() {
    use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
    use dial9_tokio_telemetry::telemetry::{TaskDumpConfig, TaskId};

    let (capture, batches) = capture_processor();
    let recorder = recorder(MemoryBuffer::new(CAPTURE_BUFFER_SIZE).unwrap())
        .with_custom_pipeline(|p| p.pipe(capture))
        .build();
    let legacy = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_dump_config(
                TaskDumpConfig::builder()
                    .idle_threshold(Duration::ZERO)
                    .build(),
            )
            .build(),
    );
    let sampled = common::attach_current_thread(
        &recorder,
        TokioAttachOptions::builder()
            .task_sampling_config(
                TaskSamplingConfig::builder()
                    .captures_per_second_per_worker(1000)
                    .build(),
            )
            .build(),
    );
    let disabled = common::attach_current_thread(&recorder, TokioAttachOptions::default());
    let handle = Dial9TokioHandle::current();
    sampled.block_on(warm_up(&handle));
    let mut tasks = Vec::new();
    for paused in [false, true] {
        for (runtime, mode) in [
            (&legacy, 1),
            (&sampled, 2),
            (&disabled, 0),
            (&legacy, 1),
            (&sampled, 2),
        ] {
            if paused {
                recorder.disable();
            }
            let task = runtime.block_on(async {
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
                let task = handle.spawn(async move {
                    started_tx.send(()).unwrap();
                    resume_rx.await.unwrap();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                });
                let id = TaskId::from(task.id()).to_u64();
                started_rx.await.unwrap();
                recorder.enable();
                resume_tx.send(()).unwrap();
                task.await.unwrap();
                id
            });
            tasks.push((task, mode));
        }
    }
    drop((legacy, sampled, disabled));
    recorder.graceful_shutdown(Duration::from_secs(1));
    let events: Vec<Dial9Event> = decode_all(&batches.lock().unwrap());
    for (task, mode) in tasks {
        let mut legacy = 0;
        let mut sampled = 0;
        for event in &events {
            match event {
                Dial9Event::TaskDumpEvent(e) if e.task_id == task => legacy += 1,
                Dial9Event::TaskSampleEvent(e) if e.task_id == task => sampled += 1,
                _ => {}
            }
        }
        assert_eq!(legacy > 0, mode == 1, "legacy events for task {task}");
        assert_eq!(sampled > 0, mode == 2, "sampled events for task {task}");
    }
}
