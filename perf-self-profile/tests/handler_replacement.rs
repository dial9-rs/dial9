//! Verifies that [`Unwinder::capture`] panics via `debug_assert!` when its
//! SIGSEGV handler has been replaced out from under it. This protects against
//! third-party signal handlers silently breaking stack-walk fault recovery.
#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use dial9_perf_self_profile::unwinder::Unwinder;

/// Skip the test if something (e.g. ASAN) replaced our SIGSEGV handler after
/// install. Returns the Unwinder on success.
fn install_or_skip() -> Option<Unwinder> {
    let u = Unwinder::install().unwrap();
    if !u.verify_handler() {
        eprintln!("skipping: SIGSEGV handler was replaced (sanitizer?)");
        return None;
    }
    Some(u)
}

/// In debug builds, `capture` panics via `debug_assert!` when its SIGSEGV
/// handler has been replaced out from under it.
#[cfg(debug_assertions)]
#[test]
fn capture_debug_asserts_when_handler_replaced() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let Some(unwinder) = install_or_skip() else {
        return;
    };
    assert!(unwinder.verify_handler());

    // Replace SIGSEGV with SIG_IGN so verify_handler() returns false.
    // SAFETY: we restore our handler below on all exit paths via the
    // guard, making this safe even under `cargo test`'s threaded harness.
    let mut new_action: libc::sigaction = unsafe { std::mem::zeroed() };
    new_action.sa_sigaction = libc::SIG_IGN;
    unsafe { libc::sigemptyset(&mut new_action.sa_mask) };
    let mut old_action: libc::sigaction = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigaction(libc::SIGSEGV, &new_action, &mut old_action) };
    assert_eq!(rc, 0, "failed to replace SIGSEGV handler");

    struct RestoreGuard(libc::sigaction);
    impl Drop for RestoreGuard {
        fn drop(&mut self) {
            // Restore the original handler so concurrent tests under
            // `cargo test` (threaded harness) still have fault recovery.
            let _ = unsafe { libc::sigaction(libc::SIGSEGV, &self.0, std::ptr::null_mut()) };
        }
    }
    let _guard = RestoreGuard(old_action);

    assert!(
        !unwinder.verify_handler(),
        "precondition: handler should appear replaced"
    );

    // capture() must panic via debug_assert before doing any stack
    // walking.
    let mut out = [0u64; 16];
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: we intentionally violate the "handler installed"
        // precondition to verify the debug_assert fires. The panic
        // from debug_assert aborts before the frame walk runs.
        let _ = unsafe { unwinder.capture(&mut out) };
    }));
    assert!(
        result.is_err(),
        "capture should panic via debug_assert when handler is replaced"
    );
}
