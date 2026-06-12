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
//! On first hook entry, both slots are initialized. Because `LIFETIME_GUARD`
//! is initialized *before* `sdd`'s internal TLS (which is lazily initialized
//! on the first `scc` operation), TLS destructors run in reverse order:
//! `sdd` drops first, then our `LIFETIME_GUARD`. After our guard drops,
//! subsequent hook entries see `IS_SHUTTING_DOWN = true` and bail out.
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

    /// Has `Drop`. Destructor flips `IS_SHUTTING_DOWN = true`.
    static LIFETIME_GUARD: Lifetime = const { Lifetime };
}

/// Returns `true` if the current thread is shutting down and `scc` operations
/// are unsafe. The hook must bail out without touching the liveset.
///
/// # Allocation-free guarantee
///
/// This function performs only TLS reads — no allocations, no locks.
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
