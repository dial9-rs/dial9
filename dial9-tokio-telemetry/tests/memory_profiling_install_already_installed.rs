#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Test that a second install() returns AlreadyInstalled.

use dial9_tokio_telemetry::memory_profiling::{InstallError, MemoryProfiler};
use dial9_tokio_telemetry::telemetry::{RecorderBuilderTokioExt, recorder};

mod common;

#[test]
fn second_install_returns_already_installed() {
    let traced = recorder(common::small_mem_writer())
        .with_tokio(|t| {
            t.worker_threads(1);
        })
        .build()
        .unwrap();

    let handle = traced.guard().handle();
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
