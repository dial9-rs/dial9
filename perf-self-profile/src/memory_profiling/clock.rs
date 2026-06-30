//! Monotonic clock for trace timestamps

/// Read monotonic time in nanoseconds.
#[cfg(target_os = "linux")]
pub(crate) fn clock_monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec; CLOCK_MONOTONIC always succeeds.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn clock_monotonic_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    (EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64).saturating_add(1)
}
