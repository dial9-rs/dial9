#[cfg(feature = "cpu-profiling")]
use crate::telemetry::format::WorkerId;
use serde::Serialize;
#[cfg(feature = "cpu-profiling")]
use std::sync::Arc;

/// What triggered a CPU sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CpuSampleSource {
    /// Periodic CPU profiling sample (frequency-based).
    CpuProfile = 0,
    /// Context switch captured by per-thread sched event tracking.
    SchedEvent = 1,
}

impl CpuSampleSource {
    /// Decode from a raw `u8` wire value.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::SchedEvent,
            _ => Self::CpuProfile,
        }
    }
}

#[cfg(feature = "cpu-profiling")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThreadName(Arc<str>);

#[cfg(feature = "cpu-profiling")]
impl ThreadName {
    pub(crate) fn new(name: String) -> Self {
        Self(name.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

/// Data for a CPU stack trace sample. Implements `Encodable` so samples can
/// be written directly into the thread-local trace buffer without an
/// intermediate enum. Interning of `thread_name` and `callchain` happens in
/// the `Encodable::encode` impl.
#[cfg(feature = "cpu-profiling")]
#[derive(Debug, Clone)]
pub(crate) struct CpuSampleData {
    pub timestamp_nanos: u64,
    pub worker_id: WorkerId,
    pub tid: u32,
    pub thread_name: Option<ThreadName>,
    pub source: CpuSampleSource,
    pub callchain: Vec<u64>,
    /// CPU the sample was taken on, if the backend could determine it.
    pub cpu: Option<u32>,
}

/// Get the OS thread ID (tid) of the calling thread via `gettid()`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn current_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn current_tid() -> u32 {
    // No gettid on non-Linux; use a thread-local counter as a unique ID.
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local! { static TID: u32 = NEXT.fetch_add(1, Ordering::Relaxed); }
    TID.with(|t| *t)
}

/// Read the calling thread's CPU time via `CLOCK_THREAD_CPUTIME_ID`.
/// This is a vDSO call on Linux (~20-40ns), no actual syscall.
#[cfg(target_os = "linux")]
pub(crate) fn thread_cpu_time_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, initialized timespec on the stack.
    // CLOCK_THREAD_CPUTIME_ID is always available on Linux and always succeeds.
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn thread_cpu_time_nanos() -> u64 {
    0
}

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

/// Per-thread scheduler stats from `/proc/<pid>/task/<tid>/schedstat`.
/// Fields: run_time_ns wait_time_ns timeslices
#[derive(Debug, Clone, Copy)]
pub(crate) struct SchedStat {
    pub wait_time_ns: u64,
    /// Raw fd backing this read, exposed for FD-lifecycle tests. Not used in production.
    #[cfg(all(test, any(target_os = "linux", target_os = "android")))]
    fd: std::os::fd::RawFd,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl SchedStat {
    /// Read schedstat for the current thread using a cached per-thread file descriptor.
    /// Opening `/proc/self/task/<tid>/schedstat` is done once per thread; subsequent reads
    /// use `pread(fd, buf, 0)` which is ~2-3x cheaper than open+read+close.
    pub(crate) fn read_current() -> std::io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

        thread_local! {
            static SCHED_FD: std::cell::RefCell<Option<OwnedFd>> = const { std::cell::RefCell::new(None) };
        }

        let fd = SCHED_FD.with(|cell| -> std::io::Result<RawFd> {
            if let Some(fd) = cell.borrow().as_ref() {
                return Ok(fd.as_raw_fd());
            }
            // First call on this thread: open the file.
            // SAFETY: SYS_gettid takes no arguments and always succeeds; unsafe is
            // required because syscall() is a raw FFI function with no type checking.
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
            let path = format!("/proc/self/task/{tid}/schedstat\0");
            // SAFETY: `path` is a valid NUL-terminated string. O_RDONLY|O_CLOEXEC
            // are valid flags. The returned fd (or -1 on error) is checked below.
            let new_fd = unsafe {
                libc::open(
                    path.as_ptr() as *const libc::c_char,
                    libc::O_RDONLY | libc::O_CLOEXEC,
                )
            };
            if new_fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: new_fd was just returned by open() and is owned by us. OwnedFd
            // takes ownership and will close it on drop (including on thread exit).
            let owned = unsafe { OwnedFd::from_raw_fd(new_fd) };
            let raw = owned.as_raw_fd();
            *cell.borrow_mut() = Some(owned);
            Ok(raw)
        })?;

        let mut buf = [0u8; 64];
        // SAFETY: `fd` is a valid open file descriptor (checked above). `buf` is a
        // live stack buffer of exactly `buf.len()` bytes. pread does not advance the
        // file offset, so concurrent calls on the same fd from other threads are safe.
        let n = unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let s = std::str::from_utf8(&buf[..n as usize]).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "bad schedstat utf8")
        })?;
        let wait_time_ns = Self::parse_wait_time_ns(s)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad schedstat"))?;
        Ok(Self {
            wait_time_ns,
            #[cfg(all(test, target_os = "linux"))]
            fd,
        })
    }

    fn parse_wait_time_ns(s: &str) -> Option<u64> {
        let mut parts = s.split_whitespace();
        let _run_time_ns: u64 = parts.next()?.parse().ok()?;
        parts.next()?.parse().ok()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
impl SchedStat {
    pub(crate) fn read_current() -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "schedstat not available on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn test_schedstat_fd_closed_on_thread_exit() {
        fn fd_target(fd: std::os::fd::RawFd) -> Option<std::path::PathBuf> {
            std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()
        }

        let (fd, opened_path) = std::thread::spawn(|| {
            let fd = SchedStat::read_current().unwrap().fd;
            let path = fd_target(fd).expect("readlink /proc/self/fd/<fd> in live thread");
            (fd, path)
        })
        .join()
        .unwrap();

        assert!(
            opened_path.to_string_lossy().ends_with("/schedstat"),
            "expected schedstat path, got {opened_path:?}"
        );

        match fd_target(fd) {
            None => { /* fd is closed - good. */ }
            Some(now) if now != opened_path => {
                // fd was closed and the slot was reused for an unrelated open
                // in another thread. That still means our OwnedFd was dropped.
            }
            Some(now) => {
                panic!("schedstat fd {fd} leaked after thread exit (still points at {now:?})")
            }
        }
    }

    #[test]
    fn poll_start_ts_monotonic_is_strictly_increasing() {
        use crate::telemetry::recorder::poll_start_ts_monotonic;
        // Rapid-fire calls should never return the same value twice.
        let mut prev = poll_start_ts_monotonic();
        for _ in 0..10_000 {
            let next = poll_start_ts_monotonic();
            assert!(
                next > prev,
                "poll_start_ts_monotonic must be strictly increasing: got {next} after {prev}"
            );
            prev = next;
        }
    }
}
