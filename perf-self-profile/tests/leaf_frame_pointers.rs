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

#[path = "common/leaf_probe.rs"]
mod leaf_probe;

use dial9_perf_self_profile::unwinder::Unwinder;
use leaf_probe::*;

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

    let leaf = leaf_frame_pointers_present();
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

/// On the architectures we decode, the answer must be conclusive.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn detection_is_conclusive_on_supported_arch() {
    assert!(leaf_frame_pointers_present().is_some());
}

/// A non-leaf must carry a frame record whenever a leaf does. This holds
/// under either `force-frame-pointers` setting and gives the probe a
/// cross-check that needs no knowledge of the build flags.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn leaf_coverage_implies_non_leaf_coverage() {
    #[inline(never)]
    extern "C" fn probe_non_leaf(x: u64) -> u64 {
        // The call makes this a non-leaf under any setting.
        std::hint::black_box(x).wrapping_add(1)
    }
    let leaf = leaf_frame_pointers_present().expect("supported arch");
    // SAFETY: reading our own executable text, as in the production path.
    let non_leaf = unsafe { decode_prologue(probe_non_leaf as *const ()) }.expect("supported arch");
    if leaf {
        assert!(
            non_leaf,
            "leaf has a frame record but non-leaf does not; probe is misreading codegen"
        );
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn recognises_frame_pointer_establishing_instructions() {
    // mov x29, sp  == add x29, sp, #0
    assert!(establishes_frame_pointer_aarch64(0x9100_03FD));
    // add x29, sp, #16
    assert!(establishes_frame_pointer_aarch64(0x9100_43FD));
}

#[cfg(target_arch = "aarch64")]
#[test]
fn rejects_unrelated_instructions() {
    // add x0, sp, #0 - wrong Rd
    assert!(!establishes_frame_pointer_aarch64(0x9100_03E0));
    // add x29, x0, #0 - wrong Rn
    assert!(!establishes_frame_pointer_aarch64(0x9100_001D));
    // stp x29, x30, [sp, #-16]! - saves the pair but does not set x29
    assert!(!establishes_frame_pointer_aarch64(0xA9BF_7BFD));
    // ret
    assert!(!establishes_frame_pointer_aarch64(RET));
    // sub sp, sp, #16
    assert!(!establishes_frame_pointer_aarch64(0xD100_43FF));
}
