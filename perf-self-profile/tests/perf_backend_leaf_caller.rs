//! Does the perf backend (the on-CPU default) report a frameless leaf's
//! immediate caller?
//!
//! Here the kernel builds the callchain (`PERF_SAMPLE_CALLCHAIN`), walking the
//! `x29` chain only, so `unwind_with_lr` never runs and the frame is re-inserted
//! by `insert_leaf_caller` instead. `perf(1)` fixes this in its own userspace
//! (`tools/perf/util/arm64-frame-pointer-unwind-support.c`), which does not
//! help a library reading the same samples.
//!
//! Needs `perf_event_paranoid <= 1`. Run with the flag absent to exercise the
//! frameless case (`RUSTFLAGS` *replaces* `.cargo/config.toml`'s value):
//!
//! ```sh
//! RUSTFLAGS="--cfg tokio_unstable" DIAL9_EXPECT_LEAVES=frameless \
//!   cargo test --release -p dial9-perf-self-profile --test perf_backend_leaf_caller -- --nocapture
//! ```
//!
//! See `common/leaf_fixture.rs` for `DIAL9_NO_SKIP` and `DIAL9_EXPECT_LEAVES`.
#![cfg(all(target_os = "linux", target_arch = "aarch64"))]

mod common;

use common::leaf_fixture as fixture;

#[test]
fn perf_backend_reports_frameless_leaf_caller() {
    fixture::run(
        true,
        "needs perf_event_paranoid <= 1 and DIAL9_FORCE_CTIMER unset",
    );
}
