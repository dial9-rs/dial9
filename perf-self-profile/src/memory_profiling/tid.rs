//! Portable OS thread-id helper for the allocator hot path.

/// Get the OS thread ID (tid) of the calling thread via `gettid()`.
///
/// Allocation-free: a vDSO/syscall on Linux, a TLS read elsewhere — safe to
/// call from inside the allocator hook.
#[cfg(target_os = "linux")]
pub(crate) fn current_tid() -> u32 {
    // SAFETY: gettid has no args, only returns the caller's TID.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn current_tid() -> u32 {
    // No gettid off Linux; hand out a stable per-thread id from a counter.
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local! { static TID: u32 = NEXT.fetch_add(1, Ordering::Relaxed); }
    TID.with(|t| *t)
}
