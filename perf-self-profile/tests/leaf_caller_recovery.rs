//! End-to-end: a sample in a frameless leaf must report its immediate caller,
//! which the `x29` chain drops and `unwind_with_lr` recovers from `x30`.
//!
//! Leaves are frameless under `-C force-frame-pointers=non-leaf`, the aarch64
//! default. This repo's `.cargo/config.toml` sets `=yes`, which hides the gap,
//! so run it as below (`RUSTFLAGS` replaces that value rather than appending):
//!
//! ```sh
//! DIAL9_FORCE_CTIMER=1 DIAL9_EXPECT_LEAVES=frameless RUSTFLAGS="--cfg tokio_unstable" \
//!   cargo test --release -p dial9-perf-self-profile --test leaf_caller_recovery
//! ```
//!
//! `DIAL9_FORCE_CTIMER=1` is required: without it `PerfSampler::start` prefers
//! the perf backend, which is a different code path (see
//! `perf_backend_leaf_caller.rs`).
//!
//! See `common/leaf_fixture.rs` for `DIAL9_NO_SKIP` and `DIAL9_EXPECT_LEAVES`.
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

mod common;

use common::leaf_fixture as fixture;

#[test]
fn frameless_leaf_reports_its_immediate_caller() {
    fixture::run(
        false,
        "needs DIAL9_FORCE_CTIMER=1 so the SIGPROF walk is used",
    );
}
