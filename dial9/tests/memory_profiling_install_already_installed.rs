#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Test that a second install() returns AlreadyInstalled.

use dial9::memory::{InstallError, MemoryProfiler};
use dial9::{Dial9HandleTokioExt, TokioAttachOptions, recorder};

mod common;

#[test]
fn second_install_returns_already_installed() {
    let recorder = recorder(common::small_mem_writer()).build();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(1);
    let _rt = recorder
        .handle()
        .attach_tokio_runtime(builder, TokioAttachOptions::default())
        .expect("attach tokio");

    let handle = recorder.handle().clone();
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
