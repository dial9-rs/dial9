//! When the frame-pointer unwinder's SIGSEGV handler chains to a previously
//! installed handler, that handler must run with the signal mask the kernel
//! would have given it: its `sa_mask`, plus SIGSEGV unless it set
//! `SA_NODEFER`. The original mask is restored afterwards.
#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use dial9_perf_self_profile::unwinder::Unwinder;

static CALLS: AtomicUsize = AtomicUsize::new(0);
static SEGV_BLOCKED: AtomicBool = AtomicBool::new(false);
static USR1_BLOCKED: AtomicBool = AtomicBool::new(false);

fn blocked() -> libc::sigset_t {
    let mut cur: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: a null `set` only queries the current mask.
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut cur) };
    assert_eq!(rc, 0, "pthread_sigmask query failed");
    cur
}

fn is_member(set: &libc::sigset_t, sig: libc::c_int) -> bool {
    // SAFETY: `set` is a valid sigset.
    unsafe { libc::sigismember(set, sig) == 1 }
}

fn record_mask() {
    let cur = blocked();
    SEGV_BLOCKED.store(is_member(&cur, libc::SIGSEGV), Ordering::SeqCst);
    USR1_BLOCKED.store(is_member(&cur, libc::SIGUSR1), Ordering::SeqCst);
    CALLS.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn previous_siginfo_handler(
    _: libc::c_int,
    _: *mut libc::siginfo_t,
    _: *mut libc::c_void,
) {
    record_mask();
}

extern "C" fn previous_plain_handler(_: libc::c_int) {
    record_mask();
}

const CHILD_ENV: &str = "DIAL9_TEST_CHAIN_MASK_CHILD";

#[test]
fn chained_siginfo_handler_runs_with_its_own_mask() {
    run_case("chained_siginfo_handler_runs_with_its_own_mask", true);
}

#[test]
fn chained_plain_handler_runs_with_its_own_mask() {
    run_case("chained_plain_handler_runs_with_its_own_mask", false);
}

/// The previous handler is captured at the first install in a process, so
/// each case re-runs itself in a child process.
fn run_case(test_name: &str, siginfo: bool) {
    if std::env::var_os(CHILD_ENV).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && out.contains("1 passed"),
            "child failed: {:?}\nstdout:\n{out}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    let mut prev: libc::sigaction = unsafe { std::mem::zeroed() };
    if siginfo {
        prev.sa_sigaction = previous_siginfo_handler as *const () as usize;
        prev.sa_flags = libc::SA_SIGINFO;
    } else {
        prev.sa_sigaction = previous_plain_handler as *const () as usize;
    }
    // SAFETY: valid sigset and sigaction; this child process owns its signal state.
    unsafe {
        libc::sigemptyset(&mut prev.sa_mask);
        libc::sigaddset(&mut prev.sa_mask, libc::SIGUSR1);
        assert_eq!(
            libc::sigaction(libc::SIGSEGV, &prev, std::ptr::null_mut()),
            0
        );
    }

    let unwinder = Unwinder::install().unwrap();
    if !unwinder.verify_handler() {
        eprintln!("skipping: SIGSEGV handler was replaced (sanitizer?)");
        return;
    }

    // Not a safe_load fault, so dial9's handler chains to `prev`.
    // SAFETY: `prev` handles SIGSEGV and returns.
    assert_eq!(unsafe { libc::raise(libc::SIGSEGV) }, 0);

    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "previous handler not chained"
    );
    assert!(
        SEGV_BLOCKED.load(Ordering::SeqCst),
        "SIGSEGV not blocked in chained handler"
    );
    assert!(
        USR1_BLOCKED.load(Ordering::SeqCst),
        "chained handler's sa_mask not applied"
    );
    let after = blocked();
    assert!(!is_member(&after, libc::SIGSEGV) && !is_member(&after, libc::SIGUSR1));
}
