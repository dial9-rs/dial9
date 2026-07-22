use dial9_tokio_telemetry::telemetry::{
    RecorderTokioExt, TokioAttachOptions, TokioHooks, recorder,
};
use std::time::Duration;

mod common;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn on_thread_start_and_stop_fire() {
    let start_count = Arc::new(AtomicUsize::new(0));
    let stop_count = Arc::new(AtomicUsize::new(0));
    let sc = start_count.clone();
    let stc = stop_count.clone();

    let num_workers = 4;
    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_thread_start(move || {
        sc.fetch_add(1, Ordering::Relaxed);
    });
    hooks.on_thread_stop(move || {
        stc.fetch_add(1, Ordering::Relaxed);
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder().tokio_hooks(hooks).build(),
            |t| {
                t.enable_all();
                t.worker_threads(num_workers);
            },
        )
        .expect("build tokio runtime");

    rt.block_on(async {
        // Ensure all workers have started by spawning work on each.
        let mut handles = Vec::new();
        for _ in 0..num_workers * 4 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    drop(rt);
    drop(recorder);

    assert!(
        start_count.load(Ordering::Relaxed) >= num_workers,
        "expected on_thread_start to fire at least {num_workers} times, got {}",
        start_count.load(Ordering::Relaxed)
    );
    assert!(
        stop_count.load(Ordering::Relaxed) >= num_workers,
        "expected on_thread_stop to fire at least {num_workers} times, got {}",
        stop_count.load(Ordering::Relaxed)
    );
}

#[test]
fn each_runtime_gets_own_hooks() {
    let count_a = Arc::new(AtomicUsize::new(0));
    let count_b = Arc::new(AtomicUsize::new(0));
    let ca = count_a.clone();
    let cb = count_b.clone();

    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks_a = TokioHooks::default();
    hooks_a.on_before_task_poll(move |_meta| {
        ca.fetch_add(1, Ordering::Relaxed);
    });
    let (recorder, runtime_a) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder()
                .runtime_name("a")
                .tokio_hooks(hooks_a)
                .build(),
            |t| {
                t.worker_threads(2);
            },
        )
        .expect("attach runtime a");

    let mut hooks_b = TokioHooks::default();
    hooks_b.on_before_task_poll(move |_meta| {
        cb.fetch_add(1, Ordering::Relaxed);
    });
    let (recorder, runtime_b) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder()
                .runtime_name("b")
                .tokio_hooks(hooks_b)
                .build(),
            |t| {
                t.worker_threads(2);
            },
        )
        .expect("attach runtime b");

    // Generate work only on runtime A
    runtime_a.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..10 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    let a_polls = count_a.load(Ordering::Relaxed);
    let b_polls_before = count_b.load(Ordering::Relaxed);

    // Generate work only on runtime B
    runtime_b.block_on(async {
        let mut handles = Vec::new();
        for _ in 0..10 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    let b_polls_after = count_b.load(Ordering::Relaxed);

    assert!(a_polls > 0, "expected runtime A poll hook to fire");
    assert_eq!(
        b_polls_before, 0,
        "expected runtime B poll hook to NOT fire during A's work"
    );
    assert!(
        b_polls_after > 0,
        "expected runtime B poll hook to fire during B's work"
    );

    drop(runtime_a);
    drop(runtime_b);
    recorder.graceful_shutdown(Duration::from_secs(1));
}

#[test]
fn on_thread_park_fires() {
    let park_count = Arc::new(AtomicUsize::new(0));
    let pc = park_count.clone();

    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_thread_park(move || {
        pc.fetch_add(1, Ordering::Relaxed);
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder().tokio_hooks(hooks).build(),
            |t| {
                t.enable_all();
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");

    rt.block_on(async {
        // Sleep to let workers park
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    drop(rt);
    drop(recorder);

    assert!(
        park_count.load(Ordering::Relaxed) > 0,
        "expected on_thread_park to fire at least once"
    );
}

#[test]
fn on_thread_unpark_fires() {
    let unpark_count = Arc::new(AtomicUsize::new(0));
    let uc = unpark_count.clone();

    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_thread_unpark(move || {
        uc.fetch_add(1, Ordering::Relaxed);
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder().tokio_hooks(hooks).build(),
            |t| {
                t.enable_all();
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");

    rt.block_on(async {
        // Generate work to trigger unparks
        let mut handles = Vec::new();
        for _ in 0..20 {
            handles.push(tokio::spawn(async {
                tokio::task::yield_now().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });

    drop(rt);
    drop(recorder);

    assert!(
        unpark_count.load(Ordering::Relaxed) > 0,
        "expected on_thread_unpark to fire at least once"
    );
}

#[test]
fn task_poll_hooks_fire() {
    let before_count = Arc::new(AtomicUsize::new(0));
    let after_count = Arc::new(AtomicUsize::new(0));
    let bc = before_count.clone();
    let ac = after_count.clone();

    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_before_task_poll(move |_meta| {
        bc.fetch_add(1, Ordering::Relaxed);
    });
    hooks.on_after_task_poll(move |_meta| {
        ac.fetch_add(1, Ordering::Relaxed);
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder().tokio_hooks(hooks).build(),
            |t| {
                t.enable_all();
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");

    rt.block_on(async {
        let handle = tokio::spawn(async {
            tokio::task::yield_now().await;
        });
        handle.await.unwrap();
    });

    drop(rt);
    drop(recorder);

    assert!(
        before_count.load(Ordering::Relaxed) > 0,
        "expected on_before_task_poll to fire"
    );
    assert!(
        after_count.load(Ordering::Relaxed) > 0,
        "expected on_after_task_poll to fire"
    );
    assert_eq!(
        before_count.load(Ordering::Relaxed),
        after_count.load(Ordering::Relaxed),
        "before and after poll counts should match"
    );
}

#[test]
fn task_lifecycle_hooks_fire() {
    let spawn_count = Arc::new(AtomicUsize::new(0));
    let terminate_count = Arc::new(AtomicUsize::new(0));
    let sc = spawn_count.clone();
    let tc = terminate_count.clone();

    let num_tasks = 10;
    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_task_spawn(move |_meta| {
        sc.fetch_add(1, Ordering::Relaxed);
    });
    hooks.on_task_terminate(move |_meta| {
        tc.fetch_add(1, Ordering::Relaxed);
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder()
                .task_tracking_enabled(true)
                .tokio_hooks(hooks)
                .build(),
            |t| {
                t.enable_all();
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");

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
    });

    drop(rt);
    drop(recorder);

    assert!(
        spawn_count.load(Ordering::Relaxed) >= num_tasks,
        "expected on_task_spawn to fire at least {num_tasks} times, got {}",
        spawn_count.load(Ordering::Relaxed)
    );
    assert!(
        terminate_count.load(Ordering::Relaxed) >= num_tasks,
        "expected on_task_terminate to fire at least {num_tasks} times, got {}",
        terminate_count.load(Ordering::Relaxed)
    );
}

#[test]
fn task_spawn_hook_fires_when_task_tracking_disabled() {
    let spawn_count = Arc::new(AtomicUsize::new(0));
    let sc = spawn_count.clone();

    let num_tasks = 5;
    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_task_spawn(move |_meta| {
        sc.fetch_add(1, Ordering::Relaxed);
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder()
                .task_tracking_enabled(false)
                .tokio_hooks(hooks)
                .build(),
            |t| {
                t.enable_all();
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");

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
    });

    drop(rt);
    drop(recorder);

    assert!(
        spawn_count.load(Ordering::Relaxed) >= num_tasks,
        "expected on_task_spawn to fire at least {num_tasks} times even with task_tracking disabled, got {}",
        spawn_count.load(Ordering::Relaxed)
    );
}

#[test]
fn task_terminate_hook_fires_when_task_tracking_disabled() {
    let terminate_count = Arc::new(AtomicUsize::new(0));
    let tc = terminate_count.clone();

    let num_tasks = 5;
    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_task_terminate(move |_meta| {
        tc.fetch_add(1, Ordering::Relaxed);
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder()
                .task_tracking_enabled(false)
                .tokio_hooks(hooks)
                .build(),
            |t| {
                t.enable_all();
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");

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
    });

    drop(rt);
    drop(recorder);

    assert!(
        terminate_count.load(Ordering::Relaxed) >= num_tasks,
        "expected on_task_terminate to fire at least {num_tasks} times even with task_tracking disabled, got {}",
        terminate_count.load(Ordering::Relaxed)
    );
}

#[test]
fn dial9_hooks_run_before_user_hooks() {
    use dial9_tokio_telemetry::telemetry::Dial9Handle;

    let hook_fired = Arc::new(AtomicUsize::new(0));
    let hf = hook_fired.clone();

    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_thread_start(move || {
        // dial9's hook must have already run and installed Dial9Handle
        let handle = Dial9Handle::current();
        assert!(
            handle.is_enabled(),
            "Dial9Handle should be installed by dial9 before user hook runs"
        );
        hf.fetch_add(1, Ordering::Relaxed);
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder().tokio_hooks(hooks).build(),
            |t| {
                t.enable_all();
                t.worker_threads(2);
            },
        )
        .expect("build tokio runtime");

    rt.block_on(async {
        tokio::spawn(async {
            tokio::task::yield_now().await;
        })
        .await
        .unwrap();
    });

    drop(rt);
    drop(recorder);

    assert!(
        hook_fired.load(Ordering::Relaxed) > 0,
        "user on_thread_start hook should have fired"
    );
}

#[test]
fn hook_stacking_single_callback_fires() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let log_c = log.clone();

    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_thread_park(move || {
        log_c.lock().unwrap().push("park_a");
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder().tokio_hooks(hooks).build(),
            |t| {
                t.enable_all();
                t.worker_threads(1);
            },
        )
        .expect("build tokio runtime");

    rt.block_on(async {
        // Sleep to let the worker park at least once
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    drop(rt);
    drop(recorder);

    let entries = log.lock().unwrap();
    assert!(
        entries.contains(&"park_a"),
        "expected single callback to fire, got: {entries:?}"
    );
}

#[test]
fn hook_stacking_multiple_callbacks_fire_in_order() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let log_a = log.clone();
    let log_b = log.clone();

    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_thread_park(move || {
        log_a.lock().unwrap().push("park_a");
    });
    hooks.on_thread_park(move || {
        log_b.lock().unwrap().push("park_b");
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder().tokio_hooks(hooks).build(),
            |t| {
                t.enable_all();
                t.worker_threads(1);
            },
        )
        .expect("build tokio runtime");

    rt.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    drop(rt);
    drop(recorder);

    let entries = log.lock().unwrap();
    assert!(
        entries.contains(&"park_a"),
        "expected first callback to fire, got: {entries:?}"
    );
    assert!(
        entries.contains(&"park_b"),
        "expected second callback to fire, got: {entries:?}"
    );
    // Verify ordering: every "park_a" should appear before its corresponding "park_b"
    let first_a = entries.iter().position(|e| *e == "park_a").unwrap();
    let first_b = entries.iter().position(|e| *e == "park_b").unwrap();
    assert!(
        first_a < first_b,
        "expected park_a before park_b, got: {entries:?}"
    );
}

#[test]
fn hook_stacking_multiple_with_tokio_hooks_calls() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let log_a = log.clone();
    let log_b = log.clone();

    let recorder = recorder(common::small_mem_writer()).build();

    // Stacking hooks the same way multiple `with_tokio_hooks` calls did: register
    // onto the same `TokioHooks` value, preserving registration order.
    let mut hooks = TokioHooks::default();
    hooks.on_thread_park(move || {
        log_a.lock().unwrap().push("call_1");
    });
    hooks.on_thread_park(move || {
        log_b.lock().unwrap().push("call_2");
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder().tokio_hooks(hooks).build(),
            |t| {
                t.enable_all();
                t.worker_threads(1);
            },
        )
        .expect("build tokio runtime");

    rt.block_on(async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    drop(rt);
    drop(recorder);

    let entries = log.lock().unwrap();
    assert!(
        entries.contains(&"call_1"),
        "expected first with_tokio_hooks callback to fire, got: {entries:?}"
    );
    assert!(
        entries.contains(&"call_2"),
        "expected second with_tokio_hooks callback to fire, got: {entries:?}"
    );
    let first_1 = entries.iter().position(|e| *e == "call_1").unwrap();
    let first_2 = entries.iter().position(|e| *e == "call_2").unwrap();
    assert!(
        first_1 < first_2,
        "expected call_1 before call_2, got: {entries:?}"
    );
}

#[test]
fn hook_stacking_task_meta_hooks_fire_in_order() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let log_a = log.clone();
    let log_b = log.clone();

    let recorder = recorder(common::small_mem_writer()).build();

    let mut hooks = TokioHooks::default();
    hooks.on_before_task_poll(move |_meta| {
        log_a.lock().unwrap().push("poll_a");
    });
    hooks.on_before_task_poll(move |_meta| {
        log_b.lock().unwrap().push("poll_b");
    });

    let (recorder, rt) = recorder
        .attach_runtime_with(
            TokioAttachOptions::builder().tokio_hooks(hooks).build(),
            |t| {
                t.enable_all();
                t.worker_threads(1);
            },
        )
        .expect("build tokio runtime");

    rt.block_on(async {
        tokio::spawn(async {
            tokio::task::yield_now().await;
        })
        .await
        .unwrap();
    });

    drop(rt);
    drop(recorder);

    let entries = log.lock().unwrap();
    assert!(
        entries.contains(&"poll_a"),
        "expected first task meta callback to fire, got: {entries:?}"
    );
    assert!(
        entries.contains(&"poll_b"),
        "expected second task meta callback to fire, got: {entries:?}"
    );
    let first_a = entries.iter().position(|e| *e == "poll_a").unwrap();
    let first_b = entries.iter().position(|e| *e == "poll_b").unwrap();
    assert!(
        first_a < first_b,
        "expected poll_a before poll_b, got: {entries:?}"
    );
}
