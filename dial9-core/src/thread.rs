//! Thread identity helpers.

/// OS thread ID (tid) of the calling thread.
///
/// `gettid()` on Linux/Android (a vDSO/syscall); a stable per-thread counter
/// elsewhere. Allocation-free, so it is safe to call from the allocator hook.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn current_tid() -> u32 {
    // SAFETY: gettid takes no args and only returns the caller's tid.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn current_tid() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local! { static TID: u32 = NEXT.fetch_add(1, Ordering::Relaxed); }
    TID.with(|t| *t)
}
