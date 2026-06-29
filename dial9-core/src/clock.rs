//! Monotonic and realtime clock readings, the single time base for all trace
//! timestamps.

/// Read monotonic time in nanoseconds for trace timestamps.
pub fn clock_monotonic_ns() -> u64 {
    clock_monotonic_ns_impl()
}

#[cfg(unix)]
fn clock_monotonic_ns_impl() -> u64 {
    clock_gettime_ns(MONOTONIC_CLOCK_ID)
}

// Matches Rust's Darwin `Instant` backend on Apple platforms.
#[cfg(all(unix, target_vendor = "apple"))]
const MONOTONIC_CLOCK_ID: libc::clockid_t = libc::CLOCK_UPTIME_RAW;

// Matches Rust's Unix `Instant` backend on non-Apple platforms.
#[cfg(all(unix, not(target_vendor = "apple")))]
const MONOTONIC_CLOCK_ID: libc::clockid_t = libc::CLOCK_MONOTONIC;

#[cfg(unix)]
fn clock_gettime_ns(clock_id: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(clock_id, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(not(unix))]
fn clock_monotonic_ns_impl() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    (EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64).saturating_add(1)
}

/// `CLOCK_REALTIME` in nanoseconds since the Unix epoch.
#[cfg(unix)]
pub(crate) fn clock_realtime_ns() -> u64 {
    clock_gettime_ns(libc::CLOCK_REALTIME)
}

#[cfg(not(unix))]
pub(crate) fn clock_realtime_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should not be before the unix epoch")
        .as_nanos() as u64
}

/// Snapshot `(monotonic_ns, realtime_ns)` as close together as possible.
/// Reads M₁ -> R -> M₂ and pairs `R` with the midpoint of M₁ and M₂ so
/// the correlation error is half the `clock_gettime` interval.
pub(crate) fn clock_pair() -> (u64, u64) {
    let m1 = clock_monotonic_ns();
    let r = clock_realtime_ns();
    let m2 = clock_monotonic_ns();
    let mono = m1 + m2.saturating_sub(m1) / 2;
    (mono, r)
}
