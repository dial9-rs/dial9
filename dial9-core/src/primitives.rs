//! Cfg-gated concurrency primitives.
//!
//! Under normal compilation this re-exports from `std`. With `--cfg shuttle`
//! it re-exports from `shuttle`, giving the shuttle scheduler control over all
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

#[cfg(all(not(shuttle), feature = "pipeline"))]
pub mod time {
    pub use tokio::time::{Instant, sleep, sleep_until};
}

// ── shuttle path (deterministic testing) ────────────────────────────────────

#[cfg(shuttle)]
pub mod sync {
    pub use shuttle::sync::atomic;
    #[allow(unused_imports)]
    pub use shuttle::sync::{Arc, Barrier, Mutex, Weak};

    /// Shuttle's `recv_timeout` ignores the timeout and blocks forever,
    /// which would stop the flush loop from ever looping. This wraps it to
    /// randomly return `Timeout` instead, so shuttle can explore multiple
    /// flush-loop cycles.
    pub mod mpsc {
        pub use shuttle::sync::mpsc::{RecvTimeoutError, SyncSender};

        pub struct Receiver<T> {
            inner: shuttle::sync::mpsc::Receiver<T>,
        }

        // SAFETY: shuttle's Receiver<T> is Send when T: Send, so the wrapper can be too.
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

#[cfg(all(shuttle, feature = "pipeline"))]
pub mod time {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub use tokio::time::Instant;

    /// Shuttle has no virtual clock. Like `primitives::thread::sleep`, this
    /// is a single scheduling point, not a real delay.
    pub fn sleep(_duration: std::time::Duration) -> Yield {
        Yield::default()
    }

    pub fn sleep_until(_deadline: Instant) -> Yield {
        Yield::default()
    }

    #[derive(Debug, Default)]
    pub struct Yield {
        yielded: bool,
    }

    impl Future for Yield {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.yielded {
                Poll::Ready(())
            } else {
                self.yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

/// `tokio::select!`, `biased;` only under `--cfg shuttle`.
///
/// `select!`'s branch tie-break otherwise calls tokio's own internal RNG,
/// which isn't shuttle-controlled.
///
/// Production keeps randomized `select!` unchanged.
#[cfg(all(shuttle, feature = "pipeline"))]
#[macro_export]
macro_rules! shuttle_select {
    ($($arms:tt)*) => {
        tokio::select! { biased; $($arms)* }
    };
}
#[cfg(all(not(shuttle), feature = "pipeline"))]
#[macro_export]
macro_rules! shuttle_select {
    ($($arms:tt)*) => {
        tokio::select! { $($arms)* }
    };
}
#[cfg(feature = "pipeline")]
pub use crate::shuttle_select;

/// Pairs a shuttle scenario with `check_pct` and `check_uncontrolled_nondeterminism`
/// Nests the scenario in its own module so `pct`/`determinism` can be fixed leaf names.
///
/// ```ignore
/// shuttle_test! {
///     num_iters = 5_000, depth = 3;
///     fn my_scenario() { /* ... */ }
/// }
/// ```
///
/// Modifiers, added after `depth = $depth`:
/// - `should_panic` -- document a known bug instead of asserting
///   correctness. Add `expect_panic = "..."` to pin the panic message (default: any panic).
///   Add `replay = "<schedule>"` to also check in a
///   `replay_known_failure` test pinning one captured failing schedule (from
///   a `pct` run's "failing schedule" output -- not `determinism`'s "failing
///   seed", which `shuttle::replay` can't take).
/// - `should_panic, flaky_sigabrt_determinism_only` -- same, but `#[ignore]`s
///   `determinism` because it's confirmed to sometimes SIGABRT the process
///   (see that arm's comment below). Confirm the crash first; don't use
///   defensively.
/// - `verify_faults_triggered` -- also asserts
///   `primitives::fs::take_faults_triggered() > 0`, so fault injection can't
///   silently stop exercising its error path.
///
/// Use `num_iters = $num_iters, determinism_only;` instead of `num_iters =
/// .., depth = ..` for a scenario with no real concurrency to explore
/// (`check_pct` panics on those). Still needs shuttle's harness whenever the
/// scenario touches a shuttle-swapped primitive.
///
/// Don't use this macro for a scenario touching real global `static` state --
/// the generated tests run concurrently and would corrupt shuttle's own
/// bookkeeping; write those by hand behind a real `std::sync::Mutex`.
///
/// `default` in place of `num_iters = .., depth = ..` picks up this
/// codebase's established budget (5,000/3 plain, 100 `determinism_only`,
/// 10,000/3 `verify_faults_triggered`). Not offered for `should_panic`: pick
/// and justify an explicit number, since real scenarios there range
/// 500-5,000 depending on how narrow the race is.
#[cfg(shuttle)]
#[macro_export]
macro_rules! shuttle_test {
    // Re-dispatches to the explicit-budget arms below, so budgets can't
    // drift out of sync.
    (default; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        $crate::shuttle_test! {
            num_iters = 5_000, depth = 3;
            $(#[$attr])* fn $name() $body
        }
    };
    (default, determinism_only; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        $crate::shuttle_test! {
            num_iters = 100, determinism_only;
            $(#[$attr])* fn $name() $body
        }
    };
    (default, verify_faults_triggered; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        $crate::shuttle_test! {
            num_iters = 10_000, depth = 3, verify_faults_triggered;
            $(#[$attr])* fn $name() $body
        }
    };
    (num_iters = $num_iters:expr, depth = $depth:expr; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            $(#[$attr])*
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
    // No `pct`: it panics on a closure with no real concurrency to schedule
    // (single-threaded, or a fork immediately joined with no
    // overlapping-runnable window). Still needs shuttle's harness via
    // `check_uncontrolled_nondeterminism` for any scenario touching a
    // shuttle-swapped primitive.
    (num_iters = $num_iters:expr, determinism_only; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            $(#[$attr])*
            fn $name() $body

            #[test]
            fn determinism() {
                shuttle::check_uncontrolled_nondeterminism($name, $num_iters);
            }
        }
    };
    (num_iters = $num_iters:expr, depth = $depth:expr, should_panic $(, expect_panic = $msg:expr)? $(, replay = $schedule:expr)?; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            $(#[$attr])*
            fn $name() $body

            #[test]
            #[should_panic $((expected = $msg))?]
            fn pct() {
                shuttle::check_pct($name, $num_iters, $depth);
            }

            #[test]
            #[should_panic $((expected = $msg))?]
            fn determinism() {
                shuttle::check_uncontrolled_nondeterminism($name, $num_iters);
            }

            $(
                /// Replays a captured failing schedule so this exact
                /// failure reproduces deterministically, without waiting on
                /// `pct`/`determinism` exploration to find it again. No
                /// `expect_panic` pin needed -- a fixed schedule can't surface
                /// an unrelated panic.
                #[test]
                #[should_panic]
                fn replay_known_failure() {
                    shuttle::replay($name, $schedule);
                }
            )?
        }
    };
    // Same as plain `should_panic`, but `#[ignore]`s only `determinism`
    // (`pct` is unaffected) -- for a scenario confirmed to sometimes SIGABRT
    // the process under shuttle.
    //
    // Root cause: `check_uncontrolled_nondeterminism` runs each schedule
    // twice (record, then replay) to verify the same tasks stay runnable;
    // `check_pct` doesn't. If a task still holds a shuttle-backed
    // `ThreadLocalBuffer` when an uncaught panic unwinds through shuttle's
    // `Execution::run`, its `Drop` runs after shuttle's `EXECUTION_STATE` is
    // already torn down and panics again mid-unwind -- SIGABRT instead of a
    // normal test failure.
    //
    // Confirm the crash first (run `determinism` alone, repeatedly) before
    // using this -- don't use it defensively. Still runnable manually with
    // `--ignored`.
    (num_iters = $num_iters:expr, depth = $depth:expr, should_panic, flaky_sigabrt_determinism_only $(, expect_panic = $msg:expr)? $(, replay = $schedule:expr)?; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            $(#[$attr])*
            fn $name() $body

            #[test]
            #[should_panic $((expected = $msg))?]
            fn pct() {
                shuttle::check_pct($name, $num_iters, $depth);
            }

            #[test]
            #[should_panic $((expected = $msg))?]
            #[ignore = "can SIGABRT the whole process under shuttle -- see shuttle_test!'s flaky_sigabrt_determinism_only arm; run manually with --ignored"]
            fn determinism() {
                shuttle::check_uncontrolled_nondeterminism($name, $num_iters);
            }

            $(
                /// Replays a captured failing schedule so this exact
                /// failure reproduces deterministically, without waiting on
                /// `pct`/`determinism` exploration to find it again. No
                /// `expect_panic` pin needed -- a fixed schedule can't surface
                /// an unrelated panic.
                #[test]
                #[should_panic]
                fn replay_known_failure() {
                    shuttle::replay($name, $schedule);
                }
            )?
        }
    };
    // Same as the plain form, but also asserts
    // `primitives::fs::take_faults_triggered() > 0` across the whole batch,
    // so a broken fault-visibility thread-local can't silently stop fault
    // injection without failing loudly. Checked inside the same
    // `pct`/`determinism` runs, not separate tests, to avoid exploring twice.
    (num_iters = $num_iters:expr, depth = $depth:expr, verify_faults_triggered; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            $(#[$attr])*
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

    fn check() -> io::Result<()> {
        let fail = match FAULT.with(|f| f.get()) {
            FaultPolicy::None => false,
            FaultPolicy::FailAll => true,
            FaultPolicy::FailProb(p) => shuttle::rand::thread_rng().gen_bool(p),
        };
        if fail {
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
