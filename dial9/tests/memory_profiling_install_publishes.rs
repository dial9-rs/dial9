#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Test that install() publishes the process-global ACTIVE state.

use dial9::memory::{MemoryProfiler, MemoryProfilingConfig, is_installed};
use dial9::{RecorderBuilderTokioExt, recorder};

mod common;

#[test]
fn install_publishes_active_inner() {
    assert!(!is_installed(), "should not be installed before install()");

    let traced = recorder(common::small_mem_writer())
        .with_tokio(|t| {
            t.worker_threads(1);
        })
        .build()
        .unwrap();

    let handle = traced.record_handle();
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
