#![cfg(all(feature = "analysis", unix))]
//! `Dial9HandleTokioExt::detach_tokio_runtime`: a recorder that outlives its
//! runtimes releases each detached one, and a detached runtime that keeps
//! running records nothing through dial9's hooks.

mod common;

use common::{capture_processor, decode_all, decode_file};
use dial9_tokio_telemetry::telemetry::analysis_events::{Dial9Event, WorkerId};
use dial9_tokio_telemetry::telemetry::{
    Dial9Handle, Dial9HandleTokioExt, DiskBuffer, RecorderPipelineExt, TokioAttachOptions,
    TokioDetachOutcome, TokioHooks, clock_monotonic_ns, recorder,
};
use rstest::rstest;
use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind, panic_any};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
enum Flavor {
    MultiThread,
    /// A multi-thread runtime with one worker: while a task is polled there, no
    /// worker hook can run.
    OneWorker,
    CurrentThread,
    Local,
}

enum AnyRuntime {
    Runtime(tokio::runtime::Runtime),
    Local(tokio::runtime::LocalRuntime),
}

impl AnyRuntime {
    fn handle(&self) -> &tokio::runtime::Handle {
        match self {
            AnyRuntime::Runtime(rt) => rt.handle(),
            AnyRuntime::Local(rt) => rt.handle(),
        }
    }

    /// Run a few traced tasks to completion, through `dial9` spawn APIs.
    fn run_tasks(&self) {
        match self {
            AnyRuntime::Runtime(rt) => dial9_tokio_telemetry::block_on(rt, async {
                let tasks: Vec<_> = (0..4)
                    .map(|_| {
                        dial9_tokio_telemetry::spawn(async {
                            for _ in 0..3 {
                                tokio::task::yield_now().await;
                            }
                        })
                    })
                    .collect();
                for t in tasks {
                    t.await.unwrap();
                }
                // The root task waits on another thread, so the worker parks.
                tokio::task::spawn_blocking(|| ()).await.unwrap();
            }),
            AnyRuntime::Local(rt) => dial9_tokio_telemetry::block_on_local(rt, async {
                let tasks: Vec<_> = (0..4)
                    .map(|_| {
                        dial9_tokio_telemetry::spawn_local(async {
                            for _ in 0..3 {
                                tokio::task::yield_now().await;
                            }
                        })
                    })
                    .collect();
                for t in tasks {
                    t.await.unwrap();
                }
                tokio::task::spawn_blocking(|| ()).await.unwrap();
            }),
        }
    }
}

fn builder(flavor: Flavor) -> tokio::runtime::Builder {
    let mut builder = match flavor {
        Flavor::MultiThread | Flavor::OneWorker => {
            let mut b = tokio::runtime::Builder::new_multi_thread();
            b.worker_threads(if matches!(flavor, Flavor::OneWorker) {
                1
            } else {
                2
            });
            b
        }
        Flavor::CurrentThread | Flavor::Local => tokio::runtime::Builder::new_current_thread(),
    };
    builder.enable_all();
    builder
}

fn attach(handle: &Dial9Handle, flavor: Flavor, name: &str) -> AnyRuntime {
    attach_with_sentinel(handle, flavor, name, &Arc::new(()))
}

/// Attach a runtime whose hooks own a clone of `alive`. Tokio keeps builder
/// hooks in the runtime's `Handle`, so the clone drops only with the last
/// `Handle`: `Arc::strong_count(alive) == 1` means the runtime and its drivers
/// are gone.
fn attach_with_sentinel(
    handle: &Dial9Handle,
    flavor: Flavor,
    name: &str,
    alive: &Arc<()>,
) -> AnyRuntime {
    let alive = alive.clone();
    let mut hooks = TokioHooks::default();
    hooks.on_thread_start(move || {
        let _ = &alive;
    });
    let options = TokioAttachOptions::builder()
        .runtime_name(name)
        .tokio_hooks(hooks)
        .build();
    match flavor {
        Flavor::Local => AnyRuntime::Local(
            handle
                .attach_tokio_local_runtime(builder(flavor), options)
                .expect("build local runtime"),
        ),
        _ => AnyRuntime::Runtime(
            handle
                .attach_tokio_runtime(builder(flavor), options)
                .expect("build runtime"),
        ),
    }
}

/// Worker ids of the poll and park events in `events` recorded in `range`.
fn worker_ids_in(events: &[Dial9Event], range: impl Fn(u64) -> bool) -> BTreeSet<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            Dial9Event::PollStartEvent(e) => Some((e.timestamp_ns, e.worker_id)),
            Dial9Event::PollEndEvent(e) => Some((e.timestamp_ns, e.worker_id)),
            Dial9Event::WorkerParkEvent(e) => Some((e.timestamp_ns, e.worker_id)),
            Dial9Event::WorkerUnparkEvent(e) => Some((e.timestamp_ns, e.worker_id)),
            _ => None,
        })
        .filter(|(ts, id)| range(*ts) && *id != WorkerId::UNKNOWN && *id != WorkerId::BLOCKING)
        .map(|(_, id)| id.as_u64())
        .collect()
}

/// The `runtime.<name>` worker ids in the last segment metadata of `events`.
fn last_runtime_metadata(events: &[Dial9Event], name: &str) -> Option<BTreeSet<u64>> {
    let key = format!("runtime.{name}");
    events
        .iter()
        .filter_map(|e| match e {
            Dial9Event::SegmentMetadataEvent(m) => m.entries.get(&key),
            _ => None,
        })
        .next_back()
        .map(|csv| csv.split(',').map(|id| id.parse().unwrap()).collect())
}

/// Replacing a runtime many times, detaching each before dropping it, releases
/// every one: dial9 holds no `Handle` that keeps a dropped runtime's I/O and
/// time drivers open.
#[rstest]
#[case::multi_thread(Flavor::MultiThread)]
#[case::current_thread(Flavor::CurrentThread)]
#[case::local(Flavor::Local)]
fn restart_loop_releases_runtimes(#[case] flavor: Flavor) {
    const RESTARTS: usize = 50;
    let recorder = recorder(common::small_mem_writer()).build();
    let handle = recorder.handle();

    for i in 0..RESTARTS {
        let alive = Arc::new(());
        let rt = attach_with_sentinel(handle, flavor, "main", &alive);
        rt.run_tasks();
        assert!(
            Arc::strong_count(&alive) >= 2,
            "restart {i}: the sentinel must observe the live runtime"
        );
        assert_eq!(
            handle.detach_tokio_runtime(rt.handle()),
            TokioDetachOutcome::Detached
        );
        drop(rt);
        assert_eq!(
            Arc::strong_count(&alive),
            1,
            "restart {i}: a detached, dropped runtime must be released"
        );
    }
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// Control for `restart_loop_releases_runtimes`: without detach the recorder
/// keeps a dropped runtime alive, so the sentinel does see dial9's reference.
#[rstest]
#[case::multi_thread(Flavor::MultiThread)]
#[case::current_thread(Flavor::CurrentThread)]
#[case::local(Flavor::Local)]
fn undetached_runtime_stays_alive(#[case] flavor: Flavor) {
    let recorder = recorder(common::small_mem_writer()).build();
    let handle = recorder.handle();
    let alive = Arc::new(());
    let rt = attach_with_sentinel(handle, flavor, "main", &alive);
    rt.run_tasks();
    drop(rt);
    assert!(
        Arc::strong_count(&alive) > 1,
        "an attached runtime stays registered after it drops"
    );
    recorder.graceful_shutdown(Duration::from_secs(1));
}

#[derive(Clone, Copy, Debug)]
enum ScopeExit {
    Normal,
    EarlyReturn,
    Unwind,
}

/// The guard releases the runtime on each way out of its scope, including the
/// driving thread's metrics cache for current-thread and local runtimes.
#[rstest]
#[case::multi_normal(Flavor::MultiThread, ScopeExit::Normal)]
#[case::multi_early_return(Flavor::MultiThread, ScopeExit::EarlyReturn)]
#[case::multi_unwind(Flavor::MultiThread, ScopeExit::Unwind)]
#[case::current_normal(Flavor::CurrentThread, ScopeExit::Normal)]
#[case::current_early_return(Flavor::CurrentThread, ScopeExit::EarlyReturn)]
#[case::current_unwind(Flavor::CurrentThread, ScopeExit::Unwind)]
#[case::local_normal(Flavor::Local, ScopeExit::Normal)]
#[case::local_early_return(Flavor::Local, ScopeExit::EarlyReturn)]
#[case::local_unwind(Flavor::Local, ScopeExit::Unwind)]
fn guard_releases_runtime_on_scope_exit(#[case] flavor: Flavor, #[case] exit: ScopeExit) {
    let recorder = recorder(common::small_mem_writer()).build();
    let handle = recorder.handle();
    let alive = Arc::new(());

    let result = catch_unwind(AssertUnwindSafe(|| -> Result<(), &'static str> {
        let rt = attach_with_sentinel(handle, flavor, "guarded", &alive);
        let _guard = handle.detach_tokio_runtime_on_drop(rt.handle());
        rt.run_tasks();
        match exit {
            ScopeExit::Normal => {}
            ScopeExit::EarlyReturn => Err("intentional early return")?,
            ScopeExit::Unwind => panic_any("intentional guard unwind"),
        }
        Ok(())
    }));
    match exit {
        ScopeExit::Normal => {
            assert_eq!(result.expect("normal exit must not unwind"), Ok(()));
        }
        ScopeExit::EarlyReturn => {
            assert_eq!(
                result.expect("early return must not unwind"),
                Err("intentional early return")
            );
        }
        ScopeExit::Unwind => {
            let payload = result.expect_err("the guarded scope must unwind");
            assert_eq!(
                payload.downcast_ref::<&str>(),
                Some(&"intentional guard unwind")
            );
        }
    }
    assert_eq!(
        Arc::strong_count(&alive),
        1,
        "guard cleanup must release the runtime after {exit:?}"
    );
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// An owning guard can outlive the runtime value and still release its drivers.
#[rstest]
#[case::multi_thread(Flavor::MultiThread)]
#[case::current_thread(Flavor::CurrentThread)]
#[case::local(Flavor::Local)]
fn guard_releases_runtime_dropped_first(#[case] flavor: Flavor) {
    let recorder = recorder(common::small_mem_writer()).build();
    let handle = recorder.handle();
    let alive = Arc::new(());
    let rt = attach_with_sentinel(handle, flavor, "guarded", &alive);
    let guard = handle.detach_tokio_runtime_on_drop(rt.handle());
    rt.run_tasks();
    drop(rt);
    assert!(
        Arc::strong_count(&alive) >= 2,
        "the guard's handle must retain the runtime until cleanup"
    );
    drop(guard);
    assert_eq!(
        Arc::strong_count(&alive),
        1,
        "guard cleanup must release the runtime and its owned handle"
    );
    recorder.graceful_shutdown(Duration::from_secs(1));
}

#[rstest]
#[case::paused(false)]
#[case::manually_detached(true)]
fn guard_detaches_while_paused_or_already_detached(#[case] manually_detach: bool) {
    let recorder = recorder(common::small_mem_writer()).build();
    let handle = recorder.handle();
    let alive = Arc::new(());
    let rt = attach_with_sentinel(handle, Flavor::CurrentThread, "guarded", &alive);
    rt.run_tasks();
    {
        let _guard = handle.detach_tokio_runtime_on_drop(rt.handle());
        if manually_detach {
            assert_eq!(
                handle.detach_tokio_runtime(rt.handle()),
                TokioDetachOutcome::Detached
            );
        } else {
            handle.disable();
        }
    }
    assert_eq!(
        handle.detach_tokio_runtime(rt.handle()),
        TokioDetachOutcome::NotTracked,
        "the guard must have removed tracking state"
    );
    drop(rt);
    assert_eq!(Arc::strong_count(&alive), 1, "the runtime must be released");
    recorder.graceful_shutdown(Duration::from_secs(1));
}

#[derive(Clone, Copy, Debug)]
enum GuardTrackingState {
    Untracked,
    InstrumentationDisabled,
    DisabledHandle,
    RecorderStopped,
}

#[rstest]
#[case::untracked(GuardTrackingState::Untracked)]
#[case::instrumentation_disabled(GuardTrackingState::InstrumentationDisabled)]
#[case::disabled_handle(GuardTrackingState::DisabledHandle)]
#[case::recorder_stopped(GuardTrackingState::RecorderStopped)]
fn guard_tolerates_untracked_or_stopped_recorder(#[case] state: GuardTrackingState) {
    let recorder = recorder(common::small_mem_writer()).build();
    let handle = recorder.handle().clone();
    let guard_handle = if matches!(state, GuardTrackingState::DisabledHandle) {
        Dial9Handle::disabled()
    } else {
        handle.clone()
    };
    let rt = if matches!(state, GuardTrackingState::Untracked) {
        builder(Flavor::CurrentThread).build().unwrap()
    } else {
        guard_handle
            .attach_tokio_runtime(
                builder(Flavor::CurrentThread),
                TokioAttachOptions::builder()
                    .tokio_instrumentation_enabled(!matches!(
                        state,
                        GuardTrackingState::InstrumentationDisabled
                    ))
                    .build(),
            )
            .unwrap()
    };
    let guard = guard_handle.detach_tokio_runtime_on_drop(rt.handle());
    if matches!(state, GuardTrackingState::RecorderStopped) {
        recorder.graceful_shutdown(Duration::from_secs(1));
    }
    drop(guard);
    assert_eq!(
        guard_handle.detach_tokio_runtime(rt.handle()),
        TokioDetachOutcome::NotTracked
    );
}

/// Detaching after the runtime dropped, through a saved `Handle` clone, still
/// releases dial9's references; the clone then keeps the runtime alive alone.
#[test]
fn detach_after_drop_via_handle_clone() {
    let recorder = recorder(common::small_mem_writer()).build();
    let handle = recorder.handle();
    let alive = Arc::new(());

    let rt = attach_with_sentinel(handle, Flavor::MultiThread, "main", &alive);
    rt.run_tasks();
    let saved = rt.handle().clone();
    drop(rt);

    assert_eq!(
        handle.detach_tokio_runtime(&saved),
        TokioDetachOutcome::Detached
    );
    assert_eq!(
        handle.detach_tokio_runtime(&saved),
        TokioDetachOutcome::NotTracked,
        "second detach"
    );
    assert!(
        Arc::strong_count(&alive) >= 2,
        "the saved clone still keeps the runtime alive"
    );

    drop(saved);
    assert_eq!(
        Arc::strong_count(&alive),
        1,
        "dropping the last Handle after detach releases the runtime"
    );
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// The thread-local metrics cache is keyed by runtime: after a cross-thread
/// detach, driving another traced runtime on the old driver thread releases
/// the old runtime's cached metrics.
#[cfg(tokio_unstable)]
#[test]
fn cross_thread_detach_released_when_thread_drives_another_runtime() {
    let recorder = recorder(common::small_mem_writer()).build();
    let handle = recorder.handle();
    let alive = Arc::new(());

    let old = attach_with_sentinel(handle, Flavor::CurrentThread, "old", &alive);
    old.run_tasks();
    let saved = old.handle().clone();
    std::thread::scope(|s| {
        let detached = s.spawn(|| handle.detach_tokio_runtime(&saved)).join();
        assert_eq!(detached.unwrap(), TokioDetachOutcome::Detached);
    });
    drop(saved);
    drop(old);
    // Documented limitation: this thread's cache still holds `old`'s metrics.
    assert!(
        Arc::strong_count(&alive) >= 2,
        "this thread's metrics cache pins the runtime it last drove"
    );

    let new = attach(handle, Flavor::CurrentThread, "new");
    new.run_tasks();
    assert_eq!(
        handle.detach_tokio_runtime(new.handle()),
        TokioDetachOutcome::Detached
    );
    drop(new);
    assert_eq!(
        Arc::strong_count(&alive),
        1,
        "driving another runtime replaces the old runtime's cached metrics"
    );
    recorder.graceful_shutdown(Duration::from_secs(1));
}

/// After detach and reattach under the same name, the trace's segment metadata
/// attributes only the live runtime's workers to that name.
#[test]
fn same_name_restart_metadata_lists_live_workers() {
    const SEAL_WAIT: Duration = Duration::from_secs(30);
    let dir = tempfile::tempdir().unwrap();
    let (capture, batches) = capture_processor();
    // Retain the whole trace: a bounded ring can evict worker events before
    // the background capture reads them on a slow runner.
    let writer = DiskBuffer::builder()
        .base_path(dir.path())
        .max_file_size(64)
        .max_total_size(u64::MAX)
        .rotation_period(Duration::from_millis(200))
        .build()
        .unwrap();
    let recorder = recorder(writer)
        .with_custom_pipeline(|pipeline| pipeline.pipe(capture))
        .build();
    let handle = recorder.handle();

    let old = attach(handle, Flavor::MultiThread, "svc");
    // Wait for a sealed segment containing the old runtime's workers, so the
    // new runtime must replace an entry the writer already holds. The shared
    // decoder only reads completed segments.
    let deadline = Instant::now() + SEAL_WAIT;
    while last_runtime_metadata(&decode_all(&batches.lock().unwrap()), "svc").is_none() {
        assert!(
            Instant::now() < deadline,
            "old runtime's metadata never reached the trace"
        );
        old.run_tasks();
        let AnyRuntime::Runtime(rt) = &old else {
            unreachable!()
        };
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(20)).await });
    }
    assert_eq!(
        handle.detach_tokio_runtime(old.handle()),
        TokioDetachOutcome::Detached
    );
    drop(old);
    let detached_at = clock_monotonic_ns();

    let new = attach(handle, Flavor::MultiThread, "svc");
    new.run_tasks();
    drop(new);
    recorder.graceful_shutdown(Duration::from_secs(5));

    let events = decode_all(&batches.lock().unwrap());
    let old_ids = worker_ids_in(&events, |ts| ts < detached_at);
    let new_ids = worker_ids_in(&events, |ts| ts >= detached_at);
    let listed = last_runtime_metadata(&events, "svc").expect("runtime.svc metadata");
    assert!(
        !old_ids.is_empty() && !new_ids.is_empty(),
        "expected worker events from both runtimes, got old {old_ids:?}, new {new_ids:?}"
    );
    assert!(
        old_ids.is_disjoint(&new_ids),
        "worker ids are never reused: old {old_ids:?}, new {new_ids:?}"
    );
    assert!(
        !listed.is_empty() && listed.is_subset(&new_ids),
        "runtime.svc must list only the live runtime's workers {new_ids:?} \
         (old runtime had {old_ids:?}), got {listed:?}"
    );
}

/// A runtime that keeps running after detach records no poll, park, spawn or
/// terminate events from dial9's hooks: tokio's park hooks, plus its task
/// hooks under `tokio_unstable` or the `WakeTraced` wrapper otherwise.
#[rstest]
#[case::multi_thread(Flavor::OneWorker)]
#[case::current_thread(Flavor::CurrentThread)]
#[case::local(Flavor::Local)]
fn running_runtime_records_nothing_after_detach(#[case] flavor: Flavor) {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();
    let recorder = recorder(writer).build();
    let handle = recorder.handle();

    let alive = Arc::new(());
    let rt = attach_with_sentinel(handle, flavor, "main", &alive);
    rt.run_tasks();
    // A traced task spawned before detach and first polled after it.
    let pending = match &rt {
        AnyRuntime::Runtime(rt) if matches!(flavor, Flavor::CurrentThread) => {
            Some(dial9_tokio_telemetry::spawn_in(rt.handle(), async {
                tokio::task::yield_now().await;
            }))
        }
        _ => None,
    };

    let detached_at = match &rt {
        // Detach from a task on the only worker, so no worker hook call can
        // straddle the detach: every event recorded afterwards observes it.
        AnyRuntime::Runtime(inner) if matches!(flavor, Flavor::OneWorker) => {
            let handle = handle.clone();
            dial9_tokio_telemetry::block_on(inner, async move {
                assert_eq!(
                    handle.detach_tokio_runtime(&tokio::runtime::Handle::current()),
                    TokioDetachOutcome::Detached
                );
                clock_monotonic_ns()
            })
        }
        // Nothing drives a current-thread runtime between `block_on` calls.
        _ => {
            assert_eq!(
                handle.detach_tokio_runtime(rt.handle()),
                TokioDetachOutcome::Detached
            );
            clock_monotonic_ns()
        }
    };

    rt.run_tasks();
    if let (Some(pending), AnyRuntime::Runtime(inner)) = (pending, &rt) {
        inner.block_on(pending).unwrap();
    }
    drop(rt);
    // Recording nothing includes caching nothing: a hook that still resolved
    // its worker would re-cache the runtime's metrics on this thread.
    assert_eq!(
        Arc::strong_count(&alive),
        1,
        "a detached runtime driven after detach is released when dropped"
    );
    recorder.graceful_shutdown(Duration::from_secs(5));

    let events: Vec<Dial9Event> = decode_file(&dir.path().join("trace.0.bin"));
    let polls_before = events
        .iter()
        .filter(|e| matches!(e, Dial9Event::PollStartEvent(p) if p.timestamp_ns < detached_at))
        .count();
    assert!(polls_before > 0, "the runtime recorded before detach");

    let after: Vec<_> = events
        .iter()
        .filter(|e| match e {
            Dial9Event::PollStartEvent(e) => e.timestamp_ns >= detached_at,
            Dial9Event::PollEndEvent(e) => e.timestamp_ns >= detached_at,
            Dial9Event::WorkerParkEvent(e) => e.timestamp_ns >= detached_at,
            Dial9Event::WorkerUnparkEvent(e) => e.timestamp_ns >= detached_at,
            Dial9Event::TaskSpawnEvent(e) => e.timestamp_ns >= detached_at,
            Dial9Event::TaskTerminateEvent(e) => e.timestamp_ns >= detached_at,
            _ => false,
        })
        .collect();
    assert!(
        after.is_empty(),
        "a detached runtime must record no hook events, got {after:?}"
    );
}
