//! Tests that user-provided `on_thread_start` / `on_thread_stop` callbacks
//! are invoked alongside dial9's internal hooks (issue #297).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dial9_tokio_telemetry::telemetry::{RotatingWriter, TelemetryHandle, TracedRuntime};

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
        .on_thread_start(move || {
            start_c.fetch_add(1, Ordering::SeqCst);
        })
        .on_thread_stop(move || {
            stop_c.fetch_add(1, Ordering::SeqCst);
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
