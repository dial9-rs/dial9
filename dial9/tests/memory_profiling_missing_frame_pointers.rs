#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Tests for the frame-pointer self-test in `MemoryProfiler::install()`: the
//! default hard-fail behavior, `fail_on_missing_frame_pointers`'s opt-out
//! (install succeeds instead of failing), and that a failed self-test always
//! logs a warning regardless of that setting.
//!
//! These only exercise the intended failure mode when the binary is built
//! WITHOUT `-C force-frame-pointers=yes` — every other job in
//! `.github/workflows/ci.yml` builds with it, so running these under a normal
//! build would (correctly) fail. `#[ignore]`d so normal `cargo test`/
//! `cargo nextest run` skip them; run explicitly (`-- --ignored`) from a job
//! whose `RUSTFLAGS` omits the flag. See the `frame-pointer-detection` CI job.

use dial9::memory::{InstallError, MemoryProfiler, MemoryProfilingConfig};
use dial9::{Dial9HandleTokioExt, TokioAttachOptions, recorder};
use std::sync::{Arc, Mutex};

mod common;

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capturing_subscriber() -> (SharedBuf, impl tracing::Subscriber) {
    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer({
            let buf = buf.clone();
            move || buf.clone()
        })
        .with_ansi(false)
        .finish();
    (buf, subscriber)
}

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

    let (buf, subscriber) = capturing_subscriber();
    let handle = recorder.handle().clone();
    let err = tracing::subscriber::with_default(subscriber, || {
        MemoryProfiler::with_defaults().install(handle)
    })
    .expect_err("install should detect missing frame pointers");

    assert!(
        matches!(err, InstallError::MissingFramePointers { .. }),
        "expected MissingFramePointers, got: {err:?}"
    );

    let logged = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        logged.contains("frame-pointer self-test failed")
            && logged.contains("fail_on_missing_frame_pointers=true"),
        "expected a warning about the failed frame-pointer check even in the \
         hard-fail path, got: {logged:?}"
    );
}

// The memory profiler is a process-global singleton -- install() only
// succeeds once per process. Both assertions (success, and the warning it
// logs) have to share the one successful install() call in this test
// binary, rather than living in separate tests that would race for it.
#[test]
#[ignore = "only meaningful when built without -C force-frame-pointers=yes"]
fn install_without_frame_pointers_succeeds_and_warns_when_fail_is_disabled() {
    let recorder = recorder(common::small_mem_writer()).build();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(1);
    let _rt = recorder
        .handle()
        .attach_tokio_runtime(builder, TokioAttachOptions::default())
        .expect("attach tokio");

    let (buf, subscriber) = capturing_subscriber();
    let handle = recorder.handle().clone();
    let config = MemoryProfilingConfig::builder()
        .fail_on_missing_frame_pointers(false)
        .build();
    let _guard = tracing::subscriber::with_default(subscriber, || {
        MemoryProfiler::from_config(config).install(handle)
    })
    .expect("install should proceed despite missing frame pointers");

    let logged = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        logged.contains("frame-pointer self-test failed")
            && logged.contains("fail_on_missing_frame_pointers=false"),
        "expected a warning about the failed frame-pointer check, got: {logged:?}"
    );
}
