//! Bridge to Android's libsigchain (ART signal-chaining library).
//!
//! On Android, ART preloads `libsigchain.so`, which claims SIGSEGV before any
//! app code runs. A plain `sigaction(SIGSEGV, ...)` is interposed by
//! libsigchain and may never actually run the way we want — in particular,
//! when ART decides the fault belongs to it (or the JVM faults concurrently),
//! libsigchain forwards to the runtime crash handler rather than chaining to
//! our handler. That breaks the `safe_load` fault-recovery used by the
//! frame-pointer unwinder.
//!
//! `AddSpecialSignalHandlerFn` is the public ART API for "I want to run
//! BEFORE libsigchain decides what to do with this signal". A special handler
//! returns `true` to claim the signal (libsigchain stops dispatch and the
//! kernel returns to our possibly-mutated ucontext) or `false` to let
//! libsigchain forward to the next handler / ART.
//!
//! For safe_load that maps exactly: if the faulting PC is inside the
//! `safe_load_start..safe_load_end` window we mutate the ucontext PC + result
//! register and return `true`; otherwise we return `false` and let ART handle
//! the real crash.

use core::ffi::{c_int, c_void};
use std::sync::OnceLock;

/// Special signal handler signature expected by libsigchain.
///
/// Return value:
///   - `true`  → handler claimed the signal. libsigchain returns to the
///               (possibly-modified) ucontext, stopping further dispatch.
///   - `false` → not ours. libsigchain forwards to ART / user handlers.
pub type SpecialSignalHandlerFn =
    extern "C" fn(signo: c_int, info: *mut libc::siginfo_t, ucontext: *mut c_void) -> bool;

/// Matches `art::SigchainAction` in `art/sigchainlib/sigchain.h`.
#[repr(C)]
pub struct SigchainAction {
    pub sc_sigaction: Option<SpecialSignalHandlerFn>,
    pub sc_mask: libc::sigset_t,
    pub sc_flags: u64,
}

type AddSpecialSignalHandlerFnPtr = unsafe extern "C" fn(c_int, *mut SigchainAction);

struct Symbols {
    add: AddSpecialSignalHandlerFnPtr,
}

fn symbols() -> Option<&'static Symbols> {
    static CACHE: OnceLock<Option<Symbols>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            // SAFETY: `dlsym(RTLD_DEFAULT, ...)` is an async-signal-safe FFI
            // lookup; we transmute only when the returned pointer is non-null.
            let add =
                unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"AddSpecialSignalHandlerFn".as_ptr()) };
            if add.is_null() {
                return None;
            }
            // SAFETY: the non-null dlsym result points at the libsigchain symbol
            // with the documented C signature.
            Some(Symbols {
                add: unsafe {
                    core::mem::transmute::<*mut c_void, AddSpecialSignalHandlerFnPtr>(add)
                },
            })
        })
        .as_ref()
}

/// Register `handler` as a special SIGSEGV handler with libsigchain.
///
/// Returns `true` on success, `false` if libsigchain isn't present (no
/// registration was performed). The registration is process-global.
///
/// # Safety
///
/// * `handler` must be re-entrant and async-signal-safe.
/// * `handler` must return `false` for faults it does not own, so libsigchain
///   can forward to ART for genuine crashes.
/// * The handler pointer must remain valid for the lifetime of the process
///   (this prototype never unregisters).
pub unsafe fn try_register(handler: SpecialSignalHandlerFn) -> bool {
    let Some(syms) = symbols() else { return false };

    // libsigchain copies the SigchainAction by value into its own table, so a
    // stack-allocated struct is fine.
    let mut action = SigchainAction {
        sc_sigaction: Some(handler),
        // SAFETY: `sigset_t` is a POD bitmap; zero-init is a valid empty mask
        // and `sigemptyset` below normalises it for any platform that wants
        // specific bit layout.
        sc_mask: unsafe { core::mem::zeroed() },
        sc_flags: 0,
    };
    // SAFETY: `&mut action.sc_mask` is a valid sigset_t pointer.
    unsafe { libc::sigemptyset(&mut action.sc_mask) };

    // SAFETY: `add` was resolved from libsigchain via dlsym to a function with
    // the matching signature; `&mut action` is a valid pointer for the call.
    unsafe { (syms.add)(libc::SIGSEGV, &mut action) };
    true
}
