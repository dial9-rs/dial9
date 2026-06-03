#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Test that a second install() returns AlreadyInstalled.

mod common;

use dial9_tokio_telemetry::memory_profiling::{InstallError, MemoryProfiler};
use dial9_tokio_telemetry::telemetry::TracedRuntime;

#[test]
fn second_install_returns_already_installed() {
    let (writer, _batches) = common::BytesCapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (_runtime, guard) = TracedRuntime::builder()
        .build_and_start_with_writer(builder, writer)
        .unwrap();

    let handle = guard.handle();
    let _mem_guard = MemoryProfiler::with_defaults()
        .install(handle.clone())
        .expect("first install should succeed");

    let err = MemoryProfiler::with_defaults()
        .install(handle)
        .expect_err("second install should fail");

    assert!(
        matches!(err, InstallError::AlreadyInstalled),
        "expected AlreadyInstalled, got: {err:?}"
    );
}
