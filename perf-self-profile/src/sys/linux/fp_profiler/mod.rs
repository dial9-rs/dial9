//! Userspace frame-pointer profiling infrastructure.
//! x86_64 and aarch64 only.

#[cfg(target_os = "android")]
pub mod android_sigchain;
#[cfg(all(target_os = "android", target_arch = "aarch64"))]
pub mod bionic_arm64;
pub mod ctimer;
pub mod sample_buffer;
pub mod unwind;

/// Sentinel returned when the load faulted.
pub const SAFE_LOAD_FAULT: usize = 0;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "dial9-fp-profiler: unsupported Linux architecture \
     (only x86_64 and aarch64 are supported)"
);

mod supported {
    use std::ptr;
    use std::sync::atomic::AtomicPtr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::SAFE_LOAD_FAULT;

    // Trampoline for a fault-tolerant load (bounded by start/end labels).

    #[cfg(target_arch = "x86_64")]
    core::arch::global_asm!(
        ".globl safe_load_start",
        ".globl safe_load_end",
        ".globl safe_load",
        ".type safe_load, @function",
        "safe_load:",
        "safe_load_start:",
        "    mov (%rdi), %rax",
        "safe_load_end:",
        "    ret",
        options(att_syntax)
    );

    #[cfg(target_arch = "aarch64")]
    core::arch::global_asm!(
        ".globl safe_load_start",
        ".globl safe_load_end",
        ".globl safe_load",
        ".type safe_load, @function",
        "safe_load:",
        "safe_load_start:",
        "    ldr x0, [x0]",
        "safe_load_end:",
        "    ret",
    );

    unsafe extern "C" {
        fn safe_load(ptr: *const usize) -> usize;
        static safe_load_start: u8;
        static safe_load_end: u8;
    }

    /// Dereference `ptr` returning its value, or `SAFE_LOAD_FAULT` if the
    /// read faulted (unmapped page, guard page, etc.).
    ///
    /// # Safety
    /// - The SIGSEGV handler must be installed via [`install_handler`] first.
    /// - Must only be called from contexts where SAFE_LOAD_FAULT is
    ///   distinguishable from a real zero.
    /// - The pointer's alignment must be appropriate for a `usize` load.
    #[inline(always)]
    pub unsafe fn load(ptr: *const usize) -> usize {
        unsafe { safe_load(ptr) }
    }

    static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);
    static OLD_HANDLER: AtomicPtr<libc::sigaction> = AtomicPtr::new(ptr::null_mut());
    /// `true` after we have successfully registered with `libsigchain`. On
    /// non-Android platforms this is always `false` and unused.
    #[cfg(target_os = "android")]
    static SIGCHAIN_REGISTERED: AtomicBool = AtomicBool::new(false);
    #[cfg(target_os = "android")]
    static DIRECT_HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

    unsafe fn install_sigaction_handler() -> Result<(), std::io::Error> {
        let mut new_action: libc::sigaction = unsafe { std::mem::zeroed() };
        new_action.sa_sigaction = sigsegv_handler as *const () as usize;
        // SA_NODEFER: safe_load may fault inside SIGPROF, without this the second
        // SIGSEGV queues instead of fires. Invariant: neither this handler nor any
        // chained handler may fault, or we recurse without bound.
        new_action.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
        unsafe { libc::sigemptyset(&mut new_action.sa_mask) };

        let old_storage = Box::into_raw(Box::new(unsafe { std::mem::zeroed::<libc::sigaction>() }));

        if unsafe { libc::sigaction(libc::SIGSEGV, &new_action, old_storage) } != 0 {
            let err = std::io::Error::last_os_error();
            unsafe { drop(Box::from_raw(old_storage)) };
            return Err(err);
        }

        OLD_HANDLER.store(old_storage, Ordering::SeqCst);
        Ok(())
    }

    /// Install our SIGSEGV handler, chaining to whatever was previously
    /// registered (or registering with libsigchain on Android).
    ///
    /// # Safety
    /// Modifies process-global signal state. Call once during initialization.
    #[cfg(not(target_os = "android"))]
    pub unsafe fn install_handler() -> Result<(), std::io::Error> {
        if HANDLER_INSTALLED.swap(true, Ordering::SeqCst) {
            return Ok(()); // already installed
        }

        if let Err(err) = unsafe { install_sigaction_handler() } {
            HANDLER_INSTALLED.store(false, Ordering::SeqCst);
            return Err(err);
        }
        Ok(())
    }

    /// Android variant: register our safe_load fault handler with ART's
    /// `libsigchain` via `AddSpecialSignalHandlerFn` so it runs BEFORE the
    /// libsigchain-owned SIGSEGV dispatcher. If libsigchain isn't loaded
    /// (e.g. a plain adb-shell binary, no ART), installs the direct handler.
    #[cfg(target_os = "android")]
    pub unsafe fn install_handler() -> Result<(), std::io::Error> {
        if HANDLER_INSTALLED.swap(true, Ordering::SeqCst) {
            return Ok(()); // already attempted
        }
        // SAFETY: `sigchain_safe_load_handler` is async-signal-safe, returns
        // `false` for non-safe_load faults, and lives for the lifetime of the
        // process.
        let ok = unsafe { super::android_sigchain::try_register(sigchain_safe_load_handler) };
        if ok {
            SIGCHAIN_REGISTERED.store(true, Ordering::SeqCst);
            tracing::info!(
                "libsigchain safe_load handler registered; frame-pointer unwinding enabled"
            );
        } else {
            if let Err(err) = unsafe { install_sigaction_handler() } {
                HANDLER_INSTALLED.store(false, Ordering::SeqCst);
                return Err(err);
            }
            DIRECT_HANDLER_INSTALLED.store(true, Ordering::SeqCst);
            tracing::info!("libsigchain not loaded; direct safe_load handler registered");
        }
        Ok(())
    }

    /// Check whether our SIGSEGV handler is still the active handler for
    /// SIGSEGV. Returns `true` if the currently-installed handler matches
    /// the one we registered in [`install_handler`].
    ///
    /// Some other code may install its own SIGSEGV handler after ours,
    /// either chaining to us or not. This function lets callers detect
    /// that case so they can reinstall or skip capture.
    ///
    /// Performs one `sigaction` syscall; not suitable for hot paths.
    #[cfg(not(target_os = "android"))]
    pub fn handler_is_installed() -> bool {
        // If we never installed, we cannot be installed.
        if !HANDLER_INSTALLED.load(Ordering::SeqCst) {
            return false;
        }

        // Query current SIGSEGV handler without modifying it.
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: passing a null `act` pointer is valid per POSIX and only
        // retrieves the current action.
        let rc = unsafe { libc::sigaction(libc::SIGSEGV, ptr::null(), &mut current) };
        if rc != 0 {
            return false;
        }

        // Compare sa_sigaction function pointer against our handler.
        let expected = sigsegv_handler as *const () as usize;
        current.sa_sigaction == expected
    }

    /// Android variant: we can't introspect libsigchain's internal table via
    /// `sigaction`, so we track our own registration state. Once
    /// `AddSpecialSignalHandlerFn` accepts our handler we assume it stays
    /// registered for the lifetime of the process.
    #[cfg(target_os = "android")]
    pub fn handler_is_installed() -> bool {
        if SIGCHAIN_REGISTERED.load(Ordering::SeqCst) {
            return true;
        }
        if !DIRECT_HANDLER_INSTALLED.load(Ordering::SeqCst) {
            return false;
        }
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::sigaction(libc::SIGSEGV, ptr::null(), &mut current) };
        rc == 0 && current.sa_sigaction == sigsegv_handler as *const () as usize
    }

    /// Shared safe_load fault-recovery logic. Returns `true` if the faulting
    /// PC was inside the `safe_load` instruction window and the ucontext was
    /// fixed up to resume at `safe_load_end` with `SAFE_LOAD_FAULT` in the
    /// result register; `false` otherwise (caller should pass the signal on).
    ///
    /// # Safety
    /// `ucontext` must be the `ucontext_t*` provided by the kernel to an
    /// SA_SIGINFO SIGSEGV handler.
    #[inline]
    unsafe fn try_fixup_safe_load(ucontext: *mut libc::c_void) -> bool {
        // SAFETY: caller passes a kernel-provided ucontext; safe_load_start/end
        // are linker-defined code labels in this module.
        unsafe {
            let pc = get_pc(ucontext);
            let start = &safe_load_start as *const u8 as usize;
            let end = &safe_load_end as *const u8 as usize;
            if pc >= start && pc < end {
                set_pc(ucontext, end);
                set_result_reg(ucontext, SAFE_LOAD_FAULT);
                true
            } else {
                false
            }
        }
    }

    /// SIGSEGV handler for `safe_load`: if the faulting PC is within the
    /// `safe_load_start..safe_load_end` instruction range, it skips the faulting
    /// load, and resumes execution.
    /// Otherwise, it chains to the previously installed handler.
    extern "C" fn sigsegv_handler(
        signo: libc::c_int,
        info: *mut libc::siginfo_t,
        ucontext: *mut libc::c_void,
    ) {
        // SAFETY: In a SA_SIGINFO SIGSEGV handler, kernel provides a valid ucontext_t.
        unsafe {
            if try_fixup_safe_load(ucontext) {
                return;
            }

            // Not ours. Chain to the previous handler.
            let old = OLD_HANDLER.load(Ordering::SeqCst);
            if !old.is_null() {
                let old_ref = &*old;
                if old_ref.sa_flags & libc::SA_SIGINFO != 0 {
                    let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                        std::mem::transmute(old_ref.sa_sigaction);
                    f(signo, info, ucontext);
                    return;
                }
                let h = old_ref.sa_sigaction;
                if h == libc::SIG_DFL {
                    // Restore default handler and re-raise so the kernel
                    // terminates the process as expected for a real SIGSEGV.
                    let mut dfl: libc::sigaction = std::mem::zeroed();
                    dfl.sa_sigaction = libc::SIG_DFL;
                    libc::sigemptyset(&mut dfl.sa_mask);
                    libc::sigaction(libc::SIGSEGV, &dfl, ptr::null_mut());
                    libc::raise(libc::SIGSEGV);
                } else if h != libc::SIG_IGN {
                    // SAFETY: SA_SIGINFO is not set, the old handler uses the 1-arg
                    // `void (*)(int)` signal-handler signature.
                    let f: extern "C" fn(libc::c_int) = std::mem::transmute(h);
                    f(signo);
                }
            }
        }
    }

    /// libsigchain "special handler" variant of [`sigsegv_handler`]. Returns
    /// `true` if it claimed the fault (safe_load recovered via ucontext
    /// mutation) — libsigchain then returns to the modified ucontext. Returns
    /// `false` for any other SIGSEGV so libsigchain can forward to ART and
    /// the genuine crash path runs unmodified.
    #[cfg(target_os = "android")]
    extern "C" fn sigchain_safe_load_handler(
        _signo: libc::c_int,
        _info: *mut libc::siginfo_t,
        ucontext: *mut libc::c_void,
    ) -> bool {
        // SAFETY: libsigchain passes the kernel-provided ucontext through unchanged.
        unsafe { try_fixup_safe_load(ucontext) }
    }

    /// Whether Android frame-pointer unwinding has safe fault recovery.
    #[cfg(target_os = "android")]
    pub fn fp_unwind_supported() -> bool {
        SIGCHAIN_REGISTERED.load(Ordering::SeqCst)
            || DIRECT_HANDLER_INSTALLED.load(Ordering::SeqCst)
    }

    // Architecture-specific ucontext access

    #[cfg(target_arch = "x86_64")]
    unsafe fn get_pc(uc: *mut libc::c_void) -> usize {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as usize }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn set_pc(uc: *mut libc::c_void, pc: usize) {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] = pc as i64 };
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn set_result_reg(uc: *mut libc::c_void, val: usize) {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.gregs[libc::REG_RAX as usize] = val as i64 };
    }

    #[cfg(all(target_arch = "aarch64", not(target_os = "android")))]
    unsafe fn get_pc(uc: *mut libc::c_void) -> usize {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.pc as usize }
    }

    #[cfg(all(target_arch = "aarch64", not(target_os = "android")))]
    unsafe fn set_pc(uc: *mut libc::c_void, pc: usize) {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.pc = pc as u64 };
    }

    #[cfg(all(target_arch = "aarch64", not(target_os = "android")))]
    unsafe fn set_result_reg(uc: *mut libc::c_void, val: usize) {
        let uc = uc as *mut libc::ucontext_t;
        unsafe { (*uc).uc_mcontext.regs[0] = val as u64 };
    }

    #[cfg(all(target_arch = "aarch64", target_os = "android"))]
    unsafe fn get_pc(uc: *mut libc::c_void) -> usize {
        unsafe { super::bionic_arm64::android_ucontext_pc(uc) as usize }
    }

    #[cfg(all(target_arch = "aarch64", target_os = "android"))]
    unsafe fn set_pc(uc: *mut libc::c_void, pc: usize) {
        unsafe { super::bionic_arm64::android_ucontext_set_pc(uc, pc as u64) };
    }

    #[cfg(all(target_arch = "aarch64", target_os = "android"))]
    unsafe fn set_result_reg(uc: *mut libc::c_void, val: usize) {
        unsafe { super::bionic_arm64::android_ucontext_set_result_reg(uc, val as u64) };
    }
}

#[cfg(target_os = "android")]
pub use supported::fp_unwind_supported;
pub use supported::{handler_is_installed, install_handler, load};
