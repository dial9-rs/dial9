#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Test that install() publishes the process-global ACTIVE state.

use dial9::memory::{MemoryProfiler, MemoryProfilingConfig, is_installed};
use dial9::{Dial9HandleTokioExt, TokioAttachOptions, recorder};

mod common;

#[test]
fn install_publishes_active_inner() {
    assert!(!is_installed(), "should not be installed before install()");

    let recorder = recorder(common::small_mem_writer()).build();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(1);
    let _rt = recorder
        .handle()
        .attach_tokio_runtime(builder, TokioAttachOptions::default())
        .expect("attach tokio");

    let handle = recorder.handle().clone();
    let _mem_guard = MemoryProfiler::from_config(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(256 * 1024)
            .rng_seed(42)
            .build(),
    )
    .install(handle)
    .expect("install should succeed");

    assert!(is_installed(), "should be installed after install()");
}
