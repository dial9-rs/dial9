//! How the frame-pointer unwinder's SIGSEGV handler chains to the handler it
//! replaced. The previous handler is captured once per process, at the first
//! install, so every case re-runs itself in a child process.
#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use dial9_perf_self_profile::unwinder::Unwinder;

const CHILD_ENV: &str = "DIAL9_TEST_SIGSEGV_CHAINING_CHILD";

/// In the parent, re-runs `test_name` in a child and returns its output. In
/// the child, returns `None` and the caller runs the case.
fn in_child(test_name: &str) -> Option<Output> {
    if std::env::var_os(CHILD_ENV).is_some() {
        return None;
    }
    Some(
        Command::new(std::env::current_exe().unwrap())
            .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap(),
    )
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_child_passed(output: &Output) {
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child failed: {}",
        describe(output),
    );
}

/// Installs `handler` as the SIGSEGV handler that dial9 will chain to.
fn set_previous(handler: usize, flags: libc::c_int, extra_mask: &[libc::c_int]) {
    let mut prev: libc::sigaction = unsafe { std::mem::zeroed() };
    prev.sa_sigaction = handler;
    prev.sa_flags = flags;
    // SAFETY: valid sigset and sigaction; the child process owns its signal state.
    unsafe {
        libc::sigemptyset(&mut prev.sa_mask);
        for &sig in extra_mask {
            libc::sigaddset(&mut prev.sa_mask, sig);
        }
        assert_eq!(
            libc::sigaction(libc::SIGSEGV, &prev, std::ptr::null_mut()),
            0
        );
    }
}

/// Installs the unwinder; `false` if a sanitizer replaced its handler.
fn install_unwinder() -> bool {
    let unwinder = Unwinder::install().unwrap();
    if !unwinder.verify_handler() {
        eprintln!("skipping: SIGSEGV handler was replaced (sanitizer?)");
        return false;
    }
    true
}

fn sigsegv_flags() -> libc::c_int {
    let mut cur: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: a null `act` only queries the current action.
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGSEGV, std::ptr::null(), &mut cur) },
        0
    );
    cur.sa_flags
}

/// A page that faults on any access.
fn guard_page() -> *mut u8 {
    // SAFETY: anonymous mapping with no address hint.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(p, libc::MAP_FAILED);
    p.cast()
}

static GUARD: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static SEGV_BLOCKED: AtomicBool = AtomicBool::new(false);
static USR1_BLOCKED: AtomicBool = AtomicBool::new(false);
static PROF_BLOCKED: AtomicBool = AtomicBool::new(false);
static SAMPLES: AtomicUsize = AtomicUsize::new(0);

fn is_blocked(sig: libc::c_int) -> bool {
    let mut cur: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: a null `set` only queries the current mask.
    unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut cur);
        libc::sigismember(&cur, sig) == 1
    }
}

// ---- The chained handler runs with the mask the kernel would give it ----

fn record_mask() {
    SEGV_BLOCKED.store(is_blocked(libc::SIGSEGV), Ordering::SeqCst);
    USR1_BLOCKED.store(is_blocked(libc::SIGUSR1), Ordering::SeqCst);
    PROF_BLOCKED.store(is_blocked(libc::SIGPROF), Ordering::SeqCst);
    CALLS.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn record_mask_siginfo(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    record_mask();
}

extern "C" fn record_mask_plain(_: libc::c_int) {
    record_mask();
}

fn check_mask(test_name: &str, handler: usize, flags: libc::c_int) {
    if let Some(output) = in_child(test_name) {
        return assert_child_passed(&output);
    }
    set_previous(handler, flags, &[libc::SIGUSR1]);
    if !install_unwinder() {
        return;
    }
    // Not a safe_load fault, so dial9's handler chains to the previous one.
    assert_eq!(unsafe { libc::raise(libc::SIGSEGV) }, 0);

    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "previous handler not chained"
    );
    assert!(USR1_BLOCKED.load(Ordering::SeqCst), "sa_mask not applied");
    assert!(PROF_BLOCKED.load(Ordering::SeqCst), "SIGPROF not blocked");
    assert_eq!(
        SEGV_BLOCKED.load(Ordering::SeqCst),
        flags & libc::SA_NODEFER == 0,
        "SIGSEGV must be blocked unless the handler set SA_NODEFER",
    );
}

#[test]
fn chained_siginfo_handler_runs_with_its_own_mask() {
    check_mask(
        "chained_siginfo_handler_runs_with_its_own_mask",
        record_mask_siginfo as *const () as usize,
        libc::SA_SIGINFO,
    );
}

#[test]
fn chained_plain_handler_runs_with_its_own_mask() {
    check_mask(
        "chained_plain_handler_runs_with_its_own_mask",
        record_mask_plain as *const () as usize,
        0,
    );
}

#[test]
fn chained_nodefer_handler_leaves_sigsegv_unblocked() {
    check_mask(
        "chained_nodefer_handler_leaves_sigsegv_unblocked",
        record_mask_siginfo as *const () as usize,
        libc::SA_SIGINFO | libc::SA_NODEFER,
    );
}

// ---- A fault inside the chained handler ends the process ----

fn fault_once() {
    if CALLS.fetch_add(1, Ordering::SeqCst) > 0 {
        // Re-entered: the recursion is not bounded.
        unsafe { libc::_exit(3) };
    }
    // SAFETY: deliberately faults on the PROT_NONE page.
    unsafe { std::ptr::write_volatile(GUARD.load(Ordering::SeqCst) as *mut u8, 1) };
}

extern "C" fn fault_siginfo(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    fault_once();
}

extern "C" fn fault_plain(_: libc::c_int) {
    fault_once();
}

fn check_fault_in_chained(test_name: &str, handler: usize, flags: libc::c_int) {
    if let Some(output) = in_child(test_name) {
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGSEGV),
            "expected a SIGSEGV kill: {}",
            describe(&output),
        );
        return;
    }
    GUARD.store(guard_page() as usize, Ordering::SeqCst);
    set_previous(handler, flags, &[]);
    if !install_unwinder() {
        // Exit the way the parent expects, as the case cannot run.
        unsafe { libc::signal(libc::SIGSEGV, libc::SIG_DFL) };
        unsafe { libc::raise(libc::SIGSEGV) };
    }
    unsafe { libc::raise(libc::SIGSEGV) };
    unreachable!("chained handler should have faulted");
}

#[test]
fn fault_in_chained_siginfo_handler_kills_process() {
    check_fault_in_chained(
        "fault_in_chained_siginfo_handler_kills_process",
        fault_siginfo as *const () as usize,
        libc::SA_SIGINFO,
    );
}

#[test]
fn fault_in_chained_plain_handler_kills_process() {
    check_fault_in_chained(
        "fault_in_chained_plain_handler_kills_process",
        fault_plain as *const () as usize,
        0,
    );
}

// ---- A SIGPROF sample during the chained handler is not killed ----

#[cfg(feature = "__internal-bench")]
extern "C" fn sample_on_sigprof(_: libc::c_int) {
    // Stands in for a ctimer sample whose frame walk reads unmapped memory.
    let page = GUARD.load(Ordering::SeqCst);
    let mut out = [0u64; 4];
    // SAFETY: the unwinder is installed; every read goes through safe_load.
    let r = unsafe { dial9_perf_self_profile::__bench_internals::unwind(0, page, page, &mut out) };
    if r.frames_written == 1 {
        SAMPLES.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(feature = "__internal-bench")]
extern "C" fn sigprof_inside(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // A sample arriving while the chained handler runs.
    unsafe { libc::raise(libc::SIGPROF) };
    CALLS.fetch_add(1, Ordering::SeqCst);
}

#[cfg(feature = "__internal-bench")]
#[test]
fn sigprof_during_chained_handler_is_recovered() {
    if let Some(output) = in_child("sigprof_during_chained_handler_is_recovered") {
        return assert_child_passed(&output);
    }
    GUARD.store(guard_page() as usize, Ordering::SeqCst);
    // SAFETY: plain handler; the child process owns its signal state.
    unsafe { libc::signal(libc::SIGPROF, sample_on_sigprof as *const () as usize) };
    set_previous(sigprof_inside as *const () as usize, libc::SA_SIGINFO, &[]);
    if !install_unwinder() {
        return;
    }
    assert_eq!(unsafe { libc::raise(libc::SIGSEGV) }, 0);
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(SAMPLES.load(Ordering::SeqCst), 1, "sample did not recover");
}

// ---- A chained handler that jumps out keeps working ----

/// Fault-recovering runtimes leave their SIGSEGV handler with `siglongjmp`.
/// `setcontext` is the sound way to do that from Rust: it switches to a fresh
/// context and restores that context's mask, as `siglongjmp` would.
#[cfg(target_env = "gnu")]
mod escape {
    use super::*;
    use std::cell::UnsafeCell;

    struct Ctx(UnsafeCell<libc::ucontext_t>);
    // SAFETY: only touched by the single test thread of the child process.
    unsafe impl Sync for Ctx {}
    static ESCAPE: Ctx = Ctx(UnsafeCell::new(unsafe { std::mem::zeroed() }));

    extern "C" fn jump_out(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
        if CALLS.fetch_add(1, Ordering::SeqCst) > 0 {
            // Reached again after jumping out: chaining still works.
            unsafe { libc::_exit(42) };
        }
        // SAFETY: ESCAPE was prepared by makecontext.
        unsafe { libc::setcontext(ESCAPE.0.get()) };
        unsafe { libc::_exit(3) };
    }

    extern "C" fn after_escape() {
        // Fault again; the chained handler must be called a second time.
        unsafe { libc::raise(libc::SIGSEGV) };
        unsafe { libc::_exit(4) };
    }

    #[test]
    fn chained_handler_that_jumps_out_handles_next_fault() {
        if let Some(output) = in_child("escape::chained_handler_that_jumps_out_handles_next_fault")
        {
            assert_eq!(output.status.code(), Some(42), "{}", describe(&output));
            return;
        }
        let stack: &'static mut [u8] = Box::leak(vec![0u8; 256 * 1024].into_boxed_slice());
        // SAFETY: ESCAPE is initialised by getcontext before makecontext; the
        // stack is leaked so it outlives the context.
        unsafe {
            let ctx = ESCAPE.0.get();
            assert_eq!(libc::getcontext(ctx), 0);
            (*ctx).uc_stack.ss_sp = stack.as_mut_ptr().cast();
            (*ctx).uc_stack.ss_size = stack.len();
            (*ctx).uc_link = std::ptr::null_mut();
            libc::makecontext(ctx, after_escape, 0);
        }
        set_previous(jump_out as *const () as usize, libc::SA_SIGINFO, &[]);
        if !install_unwinder() {
            unsafe { libc::_exit(42) };
        }
        unsafe { libc::raise(libc::SIGSEGV) };
        unsafe { libc::_exit(5) };
    }
}

// ---- A handler installed without SA_ONSTACK keeps the thread stack ----

extern "C" fn needs_thread_stack(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // Far more than a sigaltstack holds.
    let mut buf = [0u8; 64 * 1024];
    std::hint::black_box(&mut buf);
    unsafe { libc::_exit(42) };
}

#[test]
fn handler_without_onstack_runs_on_thread_stack() {
    if let Some(output) = in_child("handler_without_onstack_runs_on_thread_stack") {
        assert_eq!(output.status.code(), Some(42), "{}", describe(&output));
        return;
    }
    set_previous(
        needs_thread_stack as *const () as usize,
        libc::SA_SIGINFO,
        &[],
    );
    if !install_unwinder() {
        unsafe { libc::_exit(42) };
    }
    assert_eq!(sigsegv_flags() & libc::SA_ONSTACK, 0);

    GUARD.store(guard_page() as usize, Ordering::SeqCst);
    // A std thread, so it has a sigaltstack the handler could be moved onto.
    std::thread::spawn(|| unsafe {
        std::ptr::write_volatile(GUARD.load(Ordering::SeqCst) as *mut u8, 1);
    })
    .join()
    .unwrap();
    unreachable!("previous handler should have exited");
}
