#![cfg(feature = "memory-profiling")]
//! Test that install with a disabled handle is a no-op.

use dial9_tokio_telemetry::memory_profiling::{MemoryProfiler, MemoryProfilingConfig};
use dial9_tokio_telemetry::telemetry::Dial9Handle;

#[test]
fn install_with_disabled_handle_is_noop() {
    let handle = Dial9Handle::disabled();
    let _guard = MemoryProfiler::from_config(MemoryProfilingConfig::default())
        .install(handle)
        .expect("install with disabled handle should succeed");
    // ACTIVE should NOT be set — disabled handle short-circuits.
}

#[test]
fn install_with_disabled_handle_does_not_prevent_future_install() {
    // A disabled-handle install doesn't consume the OnceLock slot,
    // so a second disabled-handle install also succeeds.
    let handle = Dial9Handle::disabled();
    let _g1 = MemoryProfiler::with_defaults()
        .install(handle.clone())
        .expect("first disabled install should succeed");
    let _g2 = MemoryProfiler::with_defaults()
        .install(handle)
        .expect("second disabled install should also succeed");
}

#[test]
fn default_config_uses_512_kib_sample_rate() {
    let config = MemoryProfilingConfig::default();
    assert_eq!(config.sample_rate_bytes(), 512 * 1024);
    assert!(!config.track_liveset());
    assert_eq!(config.rng_seed(), None);
    assert_eq!(config.ring_capacity(), 4096);
}
