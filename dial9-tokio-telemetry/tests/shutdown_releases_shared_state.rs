//! Tearing a recorder down releases its `SharedState` and its runtimes.
//!
//! The recorder and its runtimes reference each other, so teardown drops the
//! sources to break the loop. These tests hold the loop closed.

mod common;

use dial9_tokio_telemetry::telemetry::{
    RecorderTokioExt, TokioAttachOptions, TokioHooks, recorder,
};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn graceful_shutdown_releases_shared_state() {
    let rec = recorder(common::small_mem_writer()).build();
    let weak = Arc::downgrade(rec.shared().expect("enabled recorder"));

    let (rec, rt) = rec
        .attach_runtime(|t| {
            t.worker_threads(1);
        })
        .expect("attach runtime");
    rt.block_on(async {});
    drop(rt);
    rec.graceful_shutdown(Duration::ZERO);

    assert!(
        weak.upgrade().is_none(),
        "SharedState still has {} strong refs after shutdown",
        weak.strong_count()
    );
}

/// Same, with the recorder torn down while the runtime is still alive.
#[test]
fn drop_releases_shared_state() {
    let rec = recorder(common::small_mem_writer()).build();
    let weak = Arc::downgrade(rec.shared().expect("enabled recorder"));

    let (rec, rt) = rec
        .attach_runtime(|t| {
            t.worker_threads(1);
        })
        .expect("attach runtime");
    rt.block_on(async {});
    drop(rec);
    drop(rt);

    assert!(
        weak.upgrade().is_none(),
        "SharedState still has {} strong refs after drop",
        weak.strong_count()
    );
}

/// Covers the runtime side: a capture in a hook closure, alongside the
/// `Arc<SharedState>` clones `register_hooks` puts in the same closures.
#[test]
fn hook_closures_are_released() {
    let canary = Arc::new(());
    let weak = Arc::downgrade(&canary);

    let rec = recorder(common::small_mem_writer()).build();
    let mut hooks = TokioHooks::default();
    hooks.on_thread_park(move || {
        let _ = &canary;
    });

    let (rec, rt) = rec
        .attach_runtime_with(
            TokioAttachOptions::builder().tokio_hooks(hooks).build(),
            |t| {
                t.worker_threads(1);
            },
        )
        .expect("attach runtime");
    rt.block_on(async {});
    drop(rt);
    rec.graceful_shutdown(Duration::ZERO);

    assert!(
        weak.upgrade().is_none(),
        "hook closure still held after teardown ({} refs)",
        weak.strong_count()
    );
}
