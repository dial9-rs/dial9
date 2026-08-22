#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Negative test for the frame-pointer self-test in `MemoryProfiler::install()`.
//!
//! This only exercises the intended failure mode when the binary is built
//! WITHOUT `-C force-frame-pointers=yes` — every other job in
//! `.github/workflows/ci.yml` builds with it, so running this under a normal
//! build would (correctly) fail the assertion below. `#[ignore]`d so normal
//! `cargo test`/`cargo nextest run` skip it; run explicitly (`-- --ignored`)
//! from a job whose `RUSTFLAGS` omits the flag. See the
//! `frame-pointer-detection` CI job.

use dial9::memory::{InstallError, MemoryProfiler};
use dial9::{Dial9HandleTokioExt, TokioAttachOptions, recorder};

mod common;

#[test]
#[ignore = "only meaningful when built without -C force-frame-pointers=yes"]
fn install_without_frame_pointers_returns_missing_frame_pointers() {
    let recorder = recorder(common::small_mem_writer()).build();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(1);
    let _rt = recorder
        .handle()
        .attach_tokio_runtime(builder, TokioAttachOptions::default())
        .expect("attach tokio");

    let handle = recorder.handle().clone();
    let err = MemoryProfiler::with_defaults()
        .install(handle)
        .expect_err("install should detect missing frame pointers");

    assert!(
        matches!(err, InstallError::MissingFramePointers { .. }),
        "expected MissingFramePointers, got: {err:?}"
    );
}
