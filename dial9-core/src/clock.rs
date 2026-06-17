//! Monotonic and realtime clock readings, the single time base for all trace
//! timestamps.

/// Read `CLOCK_MONOTONIC` in nanoseconds. Used as the single time base for
/// all trace timestamps (poll events, CPU samples, sched events).
#[cfg(target_os = "linux")]
pub fn clock_monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// `CLOCK_MONOTONIC` in nanoseconds. Non-Linux fallback: elapsed time
/// since the first call on this process via `Instant`.
#[cfg(not(target_os = "linux"))]
pub fn clock_monotonic_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// `CLOCK_REALTIME` in nanoseconds since the Unix epoch.
#[cfg(target_os = "linux")]
pub fn clock_realtime_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(not(target_os = "linux"))]
pub fn clock_realtime_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should not be before the unix epoch")
        .as_nanos() as u64
}

/// Snapshot `(monotonic_ns, realtime_ns)` as close together as possible.
/// Reads M₁ -> R -> M₂ and pairs `R` with the midpoint of M₁ and M₂ so
/// the correlation error is half the `clock_gettime` interval.
pub fn clock_pair() -> (u64, u64) {
    let m1 = clock_monotonic_ns();
    let r = clock_realtime_ns();
    let m2 = clock_monotonic_ns();
    let mono = m1 + m2.saturating_sub(m1) / 2;
    (mono, r)
}
