//! A thread stack overflow must still reach std's stack-overflow handler after
//! the frame-pointer unwinder installs its SIGSEGV handler. That requires the
//! handler to run on the thread's sigaltstack (`SA_ONSTACK`); otherwise the
//! kernel cannot deliver the signal and kills the process with no diagnostic.
#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::os::unix::process::ExitStatusExt;
use std::process::Command;

use dial9_perf_self_profile::unwinder::Unwinder;

const CHILD_ENV: &str = "DIAL9_TEST_STACK_OVERFLOW_CHILD";
const CHILD_TEST: &str = "stack_overflow_is_reported_after_install";

fn sigsegv_flags() -> libc::c_int {
    let mut cur: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: a null `act` only queries the current action.
    let rc = unsafe { libc::sigaction(libc::SIGSEGV, std::ptr::null(), &mut cur) };
    assert_eq!(rc, 0, "sigaction query failed");
    cur.sa_flags
}

#[test]
fn installed_handler_runs_on_altstack() {
    let unwinder = Unwinder::install().unwrap();
    if !unwinder.verify_handler() {
        eprintln!("skipping: SIGSEGV handler was replaced (sanitizer?)");
        return;
    }
    assert_ne!(sigsegv_flags() & libc::SA_ONSTACK, 0);
}

#[inline(never)]
#[allow(unconditional_recursion)]
fn recurse(n: u64) -> u64 {
    let buf = [n; 64];
    std::hint::black_box(&buf);
    recurse(n + 1).wrapping_add(buf[0])
}

/// Runs itself in a child process: the overflow aborts whichever process it
/// happens in.
#[test]
fn stack_overflow_is_reported_after_install() {
    if std::env::var_os(CHILD_ENV).is_some() {
        let _unwinder = Unwinder::install().unwrap();
        std::thread::Builder::new()
            .name("overflower".into())
            .stack_size(1 << 20)
            .spawn(|| recurse(0))
            .unwrap()
            .join()
            .unwrap();
        unreachable!("thread should have overflowed its stack");
    }

    let output = Command::new(std::env::current_exe().unwrap())
        .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.signal(),
        Some(libc::SIGABRT),
        "expected std's stack-overflow abort, got {:?}; stderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains("thread 'overflower'") && stderr.contains("has overflowed its stack"),
        "missing std's stack-overflow diagnostic; stderr:\n{stderr}",
    );
}
