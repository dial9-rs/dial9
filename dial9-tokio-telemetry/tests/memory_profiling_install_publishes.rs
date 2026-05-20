#![cfg(feature = "memory-profiling")]
#![cfg(target_os = "linux")]
//! Test that install() publishes the process-global ACTIVE state.

mod common;

use dial9_tokio_telemetry::memory_profiling::{
    MemoryProfiler, MemoryProfilingConfig, is_installed,
};
use dial9_tokio_telemetry::telemetry::TracedRuntime;

#[test]
fn install_publishes_active_inner() {
    assert!(!is_installed(), "should not be installed before install()");

    let (writer, _events) = common::CapturingWriter::new();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(1).enable_all();
    let (_runtime, guard) = TracedRuntime::builder()
        .build_and_start_with_writer(builder, writer)
        .unwrap();

    let handle = guard.handle();
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
