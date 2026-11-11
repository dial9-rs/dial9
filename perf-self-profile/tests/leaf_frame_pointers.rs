//! Leaf frame-pointer detection, and the self-test blind spot it covers.
//!
//! `-- --nocapture` prints the detected state for the current build.
//!
//! `Unwinder::install` is only implemented on Linux x86_64/aarch64 and returns
//! `ErrorKind::Unsupported` elsewhere, so gate the file the same way
//! `handler_replacement.rs` does rather than fail on the macOS runner.
#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use dial9_perf_self_profile::unwinder::Unwinder;

/// The recursion self-test cannot detect missing *leaf* frame pointers: its
/// helper makes a call, so `-C force-frame-pointers=non-leaf` (the aarch64
/// default) gives it a frame record and it passes. This asserts that blind
/// spot explicitly, so the leaf probe is not mistaken for redundant.
#[test]
fn recursion_self_test_passes_regardless_of_leaf_frame_pointers() {
    let unwinder = Unwinder::install().expect("install unwinder");
    let self_test = unwinder
        .self_test_frame_pointers()
        .expect("self-test should run");

    let leaf = Unwinder::leaf_frame_pointers_present();
    println!(
        "arch={} recursion_self_test_passed={} leaf_frame_pointers_present={leaf:?}",
        std::env::consts::ARCH,
        self_test.passed(),
    );

    // Non-leaf frames carry a frame record under both `non-leaf` and `yes`,
    // so the recursion self-test passes either way.
    assert!(
        self_test.passed(),
        "recursion self-test should pass under any frame-pointer setting that \
         covers non-leaf functions"
    );
}
