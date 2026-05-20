//! Tests that user-provided Tokio hooks are invoked alongside dial9's internal
//! hooks via the `with_tokio_hooks` API.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dial9_tokio_telemetry::telemetry::{
    RotatingWriter, TelemetryCore, TelemetryHandle, TracedRuntime,
};

fn tmp_trace_path() -> std::path::PathBuf {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.bin");
    std::mem::forget(dir);
    path
}

#[test]
fn user_on_thread_start_is_called_alongside_dial9() {
    let start_count = Arc::new(AtomicUsize::new(0));
    let stop_count = Arc::new(AtomicUsize::new(0));
    let start_c = start_count.clone();
    let stop_c = stop_count.clone();

    let num_workers = 2;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let writer = RotatingWriter::single_file(tmp_trace_path()).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_tokio_hooks(|h| {
            h.on_thread_start(move || {
                start_c.fetch_add(1, Ordering::SeqCst);
            });
            h.on_thread_stop(move || {
                stop_c.fetch_add(1, Ordering::SeqCst);
            });
        })
        .build_and_start_with_writer(builder, writer)
        .unwrap();

    // Verify dial9's internal hooks still work (TelemetryHandle is installed).
    runtime.block_on(async {
        let handle = TelemetryHandle::current();
        assert!(handle.is_enabled(), "dial9 hooks must still be installed");
    });

    drop(runtime);
    drop(guard);

    // Each worker thread should have triggered on_thread_start.
    assert!(
        start_count.load(Ordering::SeqCst) >= num_workers,
        "on_thread_start should fire at least once per worker thread, got {}",
        start_count.load(Ordering::SeqCst)
    );
    // Each worker thread should have triggered on_thread_stop.
    assert!(
        stop_count.load(Ordering::SeqCst) >= num_workers,
        "on_thread_stop should fire at least once per worker thread, got {}",
        stop_count.load(Ordering::SeqCst)
    );
}

/// Each runtime attached via `TelemetryCore::trace_runtime` gets its own
/// independent callbacks.
#[test]
fn each_runtime_gets_its_own_callbacks() {
    let trace_path = tmp_trace_path();
    let writer = RotatingWriter::single_file(&trace_path).unwrap();

    let guard = TelemetryCore::builder()
        .writer(writer)
        .trace_path(trace_path)
        .build()
        .unwrap();
    guard.enable();

    let rt1_count = Arc::new(AtomicUsize::new(0));
    let rt2_count = Arc::new(AtomicUsize::new(0));

    let c1 = rt1_count.clone();
    let c2 = rt2_count.clone();

    let mut b1 = tokio::runtime::Builder::new_multi_thread();
    b1.worker_threads(2).enable_all();
    let (rt1, _h1) = guard
        .trace_runtime("rt1")
        .with_tokio_hooks(|h| {
            h.on_thread_start(move || {
                c1.fetch_add(1, Ordering::SeqCst);
            });
        })
        .build(b1)
        .unwrap();

    let mut b2 = tokio::runtime::Builder::new_multi_thread();
    b2.worker_threads(2).enable_all();
    let (rt2, _h2) = guard
        .trace_runtime("rt2")
        .with_tokio_hooks(|h| {
            h.on_thread_start(move || {
                c2.fetch_add(1, Ordering::SeqCst);
            });
        })
        .build(b2)
        .unwrap();

    // Drive both runtimes briefly to ensure threads start.
    rt1.block_on(async { tokio::task::yield_now().await });
    rt2.block_on(async { tokio::task::yield_now().await });

    drop(rt1);
    drop(rt2);
    drop(guard);

    // Each runtime's callback should have fired independently.
    assert_eq!(
        rt1_count.load(Ordering::SeqCst),
        2,
        "rt1 callback should only fire for rt1's 2 workers"
    );
    assert_eq!(
        rt2_count.load(Ordering::SeqCst),
        2,
        "rt2 callback should only fire for rt2's 2 workers"
    );
}

/// `on_thread_park` fires alongside dial9's internal park event recording.
#[test]
fn on_thread_park_fires_for_each_park() {
    let park_count = Arc::new(AtomicUsize::new(0));
    let park_c = park_count.clone();

    let num_workers = 2;
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(num_workers).enable_all();

    let writer = RotatingWriter::single_file(tmp_trace_path()).unwrap();
    let (runtime, guard) = TracedRuntime::builder()
        .with_tokio_hooks(|h| {
            h.on_thread_park(move || {
                park_c.fetch_add(1, Ordering::SeqCst);
            });
        })
        .build_and_start_with_writer(builder, writer)
        .unwrap();

    // Drive the runtime so workers park at least once.
    runtime.block_on(async {
        tokio::task::yield_now().await;
        // Give workers time to park after the work is done.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    drop(runtime);
    drop(guard);

    // Workers should have parked at least once each.
    assert!(
        park_count.load(Ordering::SeqCst) >= num_workers,
        "on_thread_park should fire at least once per worker, got {}",
        park_count.load(Ordering::SeqCst)
    );
}
