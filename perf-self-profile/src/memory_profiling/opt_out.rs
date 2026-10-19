#![deny(clippy::arithmetic_side_effects)]
//! OPT_OUT TLS sentinel — prevents TLS-teardown panics from `scc`/`sdd`.
//!
//! When a thread is being torn down, `sdd`'s TLS slots may already be
//! destroyed. Accessing the `scc::HashIndex` after that point would panic
//! inside `sdd::Collector::current()`. This module provides a lightweight
//! guard that detects shutdown and short-circuits the hook.
//!
//! # Mechanism
//!
//! Two TLS slots:
//! - `IS_SHUTTING_DOWN`: a `Cell<bool>` with **no destructor** — reading it
//!   can never panic, even during teardown.
//! - `LIFETIME_GUARD`: a `Lifetime` struct whose `Drop` impl flips
//!   `IS_SHUTTING_DOWN = true`.
//!
//! On first hook entry, initialize `sdd`'s collector before registering
//! `LIFETIME_GUARD`. TLS destructors run in reverse order, so our guard
//! drops first and disables liveset access before `sdd` frees its collector.
//! This also covers allocator hooks invoked from `sdd`'s own destructor.
//!
//! Cost: ~1 ns (one TLS `Cell::get` + one branch).

use std::cell::Cell;

struct Lifetime;

impl Drop for Lifetime {
    fn drop(&mut self) {
        let _ = IS_SHUTTING_DOWN.try_with(|c| c.set(true));
    }
}

thread_local! {
    /// Always-safe flag. `Cell<bool>` has no destructor, so `.try_with()`
    /// never returns `Err` under normal circumstances.
    static IS_SHUTTING_DOWN: Cell<bool> = const { Cell::new(false) };

    /// Registered after sdd's TLS so shutdown is flagged before sdd drops.
    static LIFETIME_GUARD: Lifetime = {
        // Collector initialization can allocate or reclaim memory. Prevent
        // those allocator hooks from recursively initializing this slot.
        IS_SHUTTING_DOWN.with(|c| c.set(true));
        drop(scc::Guard::new());
        IS_SHUTTING_DOWN.with(|c| c.set(false));
        Lifetime
    };
}

/// Returns `true` if the current thread is shutting down and `scc` operations
/// are unsafe. The hook must bail out without touching the liveset.
///
/// # Initialization
///
/// The first call initializes sdd's collector with liveset access disabled.
/// Subsequent calls perform only TLS reads — no allocations, no locks.
#[inline]
pub(crate) fn check_shutdown() -> bool {
    let is_shutting_down = IS_SHUTTING_DOWN.try_with(|c| c.get()).unwrap_or(true);
    if is_shutting_down {
        return true;
    }

    // Ensure LIFETIME_GUARD is initialized on this thread. If it can't
    // be initialized (already in teardown), treat as shutdown.
    if LIFETIME_GUARD.try_with(|_| ()).is_err() {
        let _ = IS_SHUTTING_DOWN.try_with(|c| c.set(true));
        return true;
    }

    false
}
