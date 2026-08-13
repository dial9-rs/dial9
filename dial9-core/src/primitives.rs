//! Cfg-gated concurrency primitives.
//!
//! Under normal compilation this re-exports from `std`. With `--cfg shuttle`
//! it re-exports from [`shuttle`], giving the shuttle scheduler control over all
//! synchronization points so that tests can explore thread interleavings
//! deterministically.

// ── std path (production) ───────────────────────────────────────────────────

#[cfg(not(shuttle))]
pub mod sync {
    pub use std::sync::atomic;
    pub use std::sync::mpsc;
    #[allow(unused_imports)]
    pub use std::sync::{Arc, Barrier, Mutex, Weak};
}

#[cfg(not(shuttle))]
pub mod thread {
    #[allow(unused_imports)]
    pub use std::thread::{JoinHandle, sleep, spawn};

    /// Spawn a named thread. Uses `std::thread::Builder` in production,
    /// falls back to plain `spawn` under shuttle (which has no Builder).
    pub fn spawn_named<F, T>(name: &str, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(f)
            .expect("failed to spawn thread")
    }
}

#[cfg(not(shuttle))]
#[macro_export]
macro_rules! define_thread_local {
    ($($tt:tt)*) => { std::thread_local! { $($tt)* } };
}
#[cfg(not(shuttle))]
pub use crate::define_thread_local as thread_local;

// ── shuttle path (deterministic testing) ────────────────────────────────────

#[cfg(shuttle)]
pub mod sync {
    pub use shuttle::sync::atomic;
    #[allow(unused_imports)]
    pub use shuttle::sync::{Arc, Barrier, Mutex, Weak};

    /// Wrapper around shuttle's mpsc that adds random timeouts to
    /// `recv_timeout`. Shuttle's built-in `recv_timeout` ignores the
    /// timeout and blocks unconditionally, which means the flush loop
    /// never loops. This wrapper randomly returns `Timeout` so shuttle
    /// can explore interleavings where the flush loop actually runs
    /// multiple cycles.
    pub mod mpsc {
        pub use shuttle::sync::mpsc::{RecvTimeoutError, SyncSender};

        pub struct Receiver<T> {
            inner: shuttle::sync::mpsc::Receiver<T>,
        }

        // shuttle::sync::mpsc::Receiver is Send but the wrapper needs to be too
        // SAFETY: shuttle's Receiver<T> is Send when T: Send
        unsafe impl<T: Send> Send for Receiver<T> {}

        impl<T> Receiver<T> {
            pub fn recv_timeout(
                &self,
                _timeout: std::time::Duration,
            ) -> Result<T, RecvTimeoutError> {
                // Randomly decide whether to simulate a timeout, giving
                // the flush loop a chance to execute its body.
                if shuttle::rand::thread_rng().gen_bool(0.8) {
                    match self.inner.try_recv() {
                        Ok(val) => Ok(val),
                        Err(shuttle::sync::mpsc::TryRecvError::Empty) => {
                            Err(RecvTimeoutError::Timeout)
                        }
                        Err(shuttle::sync::mpsc::TryRecvError::Disconnected) => {
                            Err(RecvTimeoutError::Disconnected)
                        }
                    }
                } else {
                    // Delegate to shuttle's blocking recv to explore the
                    // "flush loop blocks waiting for command" path.
                    self.inner
                        .recv()
                        .map_err(|_| RecvTimeoutError::Disconnected)
                }
            }

            pub fn recv(&self) -> Result<T, shuttle::sync::mpsc::RecvError> {
                self.inner.recv()
            }
        }

        use shuttle::rand::Rng;

        /// Wraps shuttle's `sync_channel` to return our `Receiver` wrapper.
        pub fn sync_channel<T>(bound: usize) -> (SyncSender<T>, Receiver<T>) {
            let (tx, rx) = shuttle::sync::mpsc::sync_channel(bound);
            (tx, Receiver { inner: rx })
        }
    }
}

#[cfg(shuttle)]
pub mod thread {
    #[allow(unused_imports)]
    pub use shuttle::thread::{JoinHandle, sleep, spawn};

    pub fn spawn_named<F, T>(_name: &str, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        spawn(f)
    }
}

#[cfg(shuttle)]
#[macro_export]
macro_rules! define_thread_local {
    ($($tt:tt)*) => { shuttle::thread_local! { $($tt)* } };
}
#[cfg(shuttle)]
pub use crate::define_thread_local as thread_local;

/// Pairs a shuttle scenario with both `check_pct` and
/// `check_uncontrolled_nondeterminism`, so a new test can't add one without
/// the other. Nests the scenario in its own module so `pct`/`determinism`
/// can be fixed leaf names without needing identifier concatenation.
///
/// ```ignore
/// shuttle_test! {
///     num_iters = 5_000, depth = 3;
///     fn my_scenario() { /* ... */ }
/// }
/// ```
///
/// Add `, should_panic` after `depth = $depth` to document a known,
/// reproduced bug instead of asserting correctness. Add
/// `, verify_faults_triggered` instead to also assert that
/// `primitives::fs::take_faults_triggered()` saw at least one fault during
/// the run, so a scenario that injects faults can't silently stop
/// exercising its error path without failing.
///
/// For a scenario with no real concurrency to explore (e.g. a deterministic
/// data-structure property like eviction order, or a fork-then-immediately-join
/// pattern with no overlapping-runnable window) use `num_iters = $num_iters,
/// determinism_only;` instead of `num_iters/depth` — `check_pct` panics
/// ("test closure did not exercise any concurrency") on a closure like that,
/// so only `check_uncontrolled_nondeterminism` is generated. Still needs to
/// run inside shuttle's harness (not a plain `#[test]`) whenever the scenario
/// touches a shuttle-swapped primitive (`primitives::sync::Mutex`, a
/// shuttle-only stand-in like `BoundedQueue`, etc.) — those crash immediately
/// outside shuttle's scheduler regardless of whether real concurrency is
/// involved.
/// Do not use this macro for a scenario that touches real
/// global `static` state — the generated `pct`/`determinism` tests run
/// concurrently and would corrupt shuttle's own per-primitive bookkeeping if
/// they shared such state; write those by hand, serialized behind a real
/// `std::sync::Mutex`.
#[cfg(shuttle)]
#[macro_export]
macro_rules! shuttle_test {
    (num_iters = $num_iters:expr, depth = $depth:expr; fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            fn $name() $body

            #[test]
            fn pct() {
                shuttle::check_pct($name, $num_iters, $depth);
            }

            #[test]
            fn determinism() {
                shuttle::check_uncontrolled_nondeterminism($name, $num_iters);
            }
        }
    };
    // No `pct`: `check_pct` panics on a closure with no real concurrency to
    // schedule between (either genuinely single-threaded, or a fork
    // immediately followed by a join with no overlapping-runnable window).
    // Still runs inside shuttle's harness via `check_uncontrolled_nondeterminism`,
    // which every scenario touching a shuttle-swapped primitive needs
    // regardless of whether there's anything to interleave.
    (num_iters = $num_iters:expr, determinism_only; fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            fn $name() $body

            #[test]
            fn determinism() {
                shuttle::check_uncontrolled_nondeterminism($name, $num_iters);
            }
        }
    };
    (num_iters = $num_iters:expr, depth = $depth:expr, should_panic; fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            fn $name() $body

            #[test]
            #[should_panic]
            fn pct() {
                shuttle::check_pct($name, $num_iters, $depth);
            }

            #[test]
            #[should_panic]
            fn determinism() {
                shuttle::check_uncontrolled_nondeterminism($name, $num_iters);
            }
        }
    };
    // Same as the plain form, but also asserts that
    // `primitives::fs::take_faults_triggered()` saw at least one fault
    // across the whole batch of iterations, so a broken fault-visibility
    // thread-local (or any other regression that silently stops fault
    // injection from reaching the flush thread) fails loudly instead of
    // leaving the scenario exercising no error path at all. Checked in the
    // same `pct`/`determinism` runs, not separate tests, so the shuttle
    // exploration isn't run twice per scenario.
    (num_iters = $num_iters:expr, depth = $depth:expr, verify_faults_triggered; fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            fn $name() $body

            fn assert_faults_were_triggered() {
                assert!(
                    $crate::primitives::fs::take_faults_triggered() > 0,
                    "no run across {} iterations triggered a single fault; fault injection is \
                     not reaching the flush thread (e.g. a broken fault-visibility thread-local), \
                     so this test is not exercising any error path.",
                    $num_iters,
                );
            }

            #[test]
            fn pct() {
                $crate::primitives::fs::take_faults_triggered(); // drain any count left over from an earlier test
                shuttle::check_pct($name, $num_iters, $depth);
                assert_faults_were_triggered();
            }

            #[test]
            fn determinism() {
                $crate::primitives::fs::take_faults_triggered(); // drain any count left over from an earlier test
                shuttle::check_uncontrolled_nondeterminism($name, $num_iters);
                assert_faults_were_triggered();
            }
        }
    };
}

#[cfg(not(shuttle))]
pub mod fs {
    use std::io::{self, Write};
    use std::path::Path;

    pub fn create_dir_all(path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
    pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    pub fn remove_file(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    pub fn remove_dir(path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }
    pub fn read_dir(path: &Path) -> io::Result<std::fs::ReadDir> {
        std::fs::read_dir(path)
    }
    pub fn metadata(path: &Path) -> io::Result<std::fs::Metadata> {
        std::fs::metadata(path)
    }
    pub fn read(path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    /// Active-segment file handle.
    #[derive(Debug)]
    pub struct File(std::fs::File);

    impl File {
        pub fn create(path: &Path) -> io::Result<File> {
            std::fs::File::create(path).map(File)
        }
    }

    impl Write for File {
        #[inline]
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        #[inline]
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }
}

#[cfg(shuttle)]
pub mod fs {
    use std::cell::Cell;
    use std::io::{self, ErrorKind, Write};
    use std::path::Path;

    use shuttle::rand::Rng;

    /// How to fail filesystem operations during a shuttle run.
    #[derive(Clone, Copy, Debug)]
    pub enum FaultPolicy {
        /// Delegate to real `std::fs`, nothing fails.
        None,
        /// Every fallible op returns `PermissionDenied`.
        FailAll,
        /// Each op independently fails with this probability, drawn from
        /// shuttle's RNG so the scheduler explores the fault schedule.
        FailProb(f64),
    }

    std::thread_local! {
        static FAULT: Cell<FaultPolicy> = const { Cell::new(FaultPolicy::None) };
    }

    /// Arm `policy`: the returned guard restores the previous one on drop so a
    /// fault can't leak into the next shuttle iteration.
    #[must_use]
    pub fn set_fault(policy: FaultPolicy) -> FaultGuard {
        let prev = FAULT.with(|f| f.replace(policy));
        FaultGuard { prev }
    }

    pub struct FaultGuard {
        prev: FaultPolicy,
    }

    impl Drop for FaultGuard {
        fn drop(&mut self) {
            FAULT.with(|f| f.set(self.prev));
        }
    }

    /// Count of fault checks that actually failed, independent of
    /// `rate_limited!`'s real-wall-clock gate on the resulting log line (that
    /// gate is a `static` keyed by call site, shared by every shuttle
    /// iteration *and* every test in the same test binary — a burst of
    /// iterations within its 60s window can log only the first occurrence).
    /// Lets a test assert "fault injection actually reached the flush
    /// thread" without depending on whether a warning happened to get past
    /// that unrelated, real-time-gated layer.
    #[cfg(test)]
    pub(crate) static FAULTS_TRIGGERED: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    #[cfg(test)]
    pub(crate) fn take_faults_triggered() -> u64 {
        FAULTS_TRIGGERED.swap(0, std::sync::atomic::Ordering::Relaxed)
    }

    fn check() -> io::Result<()> {
        let fail = match FAULT.with(|f| f.get()) {
            FaultPolicy::None => false,
            FaultPolicy::FailAll => true,
            FaultPolicy::FailProb(p) => shuttle::rand::thread_rng().gen_bool(p),
        };
        if fail {
            #[cfg(test)]
            FAULTS_TRIGGERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(io::Error::from(ErrorKind::PermissionDenied))
        } else {
            Ok(())
        }
    }

    pub fn create_dir_all(path: &Path) -> io::Result<()> {
        check()?;
        std::fs::create_dir_all(path)
    }
    pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
        check()?;
        std::fs::rename(from, to)
    }
    pub fn remove_file(path: &Path) -> io::Result<()> {
        check()?;
        std::fs::remove_file(path)
    }
    pub fn remove_dir(path: &Path) -> io::Result<()> {
        check()?;
        std::fs::remove_dir(path)
    }
    pub fn read_dir(path: &Path) -> io::Result<std::fs::ReadDir> {
        check()?;
        std::fs::read_dir(path)
    }
    pub fn metadata(path: &Path) -> io::Result<std::fs::Metadata> {
        check()?;
        std::fs::metadata(path)
    }
    pub fn read(path: &Path) -> io::Result<Vec<u8>> {
        check()?;
        std::fs::read(path)
    }

    /// Active-segment file handle whose `write`/`flush` honor the armed fault
    /// policy. `create` is never faulted, so a writer can always be built.
    #[derive(Debug)]
    pub struct File(std::fs::File);

    impl File {
        pub fn create(path: &Path) -> io::Result<File> {
            std::fs::File::create(path).map(File)
        }
    }

    impl Write for File {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            check()?;
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            check()?;
            self.0.flush()
        }
    }
}

// ── BoundedQueue ────────────────────────────────────────────────────────────

/// A bounded MPMC queue. Production uses `crossbeam_queue::ArrayQueue`;
/// under shuttle it uses a `Mutex<VecDeque>` so the scheduler can control
/// access.
#[cfg(not(shuttle))]
pub struct BoundedQueue<T> {
    inner: crossbeam_queue::ArrayQueue<T>,
}

#[cfg(not(shuttle))]
impl<T> std::fmt::Debug for BoundedQueue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedQueue")
            .field("len", &self.inner.len())
            .field("capacity", &self.inner.capacity())
            .finish()
    }
}

#[cfg(not(shuttle))]
impl<T> BoundedQueue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: crossbeam_queue::ArrayQueue::new(capacity),
        }
    }

    /// Push a value, evicting the oldest if full. Returns the evicted value.
    pub fn force_push(&self, value: T) -> Option<T> {
        self.inner.force_push(value)
    }

    pub fn pop(&self) -> Option<T> {
        self.inner.pop()
    }
}

#[cfg(shuttle)]
pub struct BoundedQueue<T> {
    inner: shuttle::sync::Mutex<std::collections::VecDeque<T>>,
    capacity: usize,
}

#[cfg(shuttle)]
impl<T> std::fmt::Debug for BoundedQueue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedQueue")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

#[cfg(shuttle)]
impl<T> BoundedQueue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: shuttle::sync::Mutex::new(std::collections::VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn force_push(&self, value: T) -> Option<T> {
        let mut q = self.inner.lock().unwrap();
        let evicted = if q.len() >= self.capacity {
            q.pop_front()
        } else {
            None
        };
        q.push_back(value);
        evicted
    }

    pub fn pop(&self) -> Option<T> {
        self.inner.lock().unwrap().pop_front()
    }
}
