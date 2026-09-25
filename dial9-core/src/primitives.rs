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
    pub use tokio::time::error::Elapsed;
    pub use tokio::time::{Instant, sleep, sleep_until, timeout};

    pub fn now() -> Instant {
        Instant::now()
    }

    pub fn elapsed_since(t: std::time::SystemTime) -> std::time::Duration {
        t.elapsed().unwrap_or_default()
    }
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
    use std::cell::Cell;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub use tokio::time::Instant;

    // `Instant::now()` is nondeterministic under shuttle replay (real
    // clock). `now()` reads a thread-local logical clock instead, advanced
    // one tick per genuine `Yield` suspend, isolated per concurrently
    // running `#[test]`.
    std::thread_local! {
        static LOGICAL_CLOCK: (Instant, Cell<u64>) = (Instant::now(), Cell::new(0));
    }

    // Comfortably clears this crate's current millisecond-scale test
    // deadlines; not derived from anything principled.
    const LOGICAL_TICK: std::time::Duration = std::time::Duration::from_millis(10);

    pub fn now() -> Instant {
        LOGICAL_CLOCK.with(|(base, nanos)| *base + std::time::Duration::from_nanos(nanos.get()))
    }

    /// Always zero: `SystemTime::elapsed()` needs a real clock read, which
    /// is nondeterministic under shuttle replay. Matches `sleep`/
    /// `sleep_until` below, which also ignore their requested duration
    /// rather than fake one.
    pub fn elapsed_since(_t: std::time::SystemTime) -> std::time::Duration {
        std::time::Duration::ZERO
    }

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

    // Accumulates across a whole `check_pct`/determinism batch (shuttle
    // resets its own state every iteration). Thread-local so a
    // concurrently-running `#[test]`, on its own OS thread, can't inflate
    // this one's count.
    std::thread_local! {
        static YIELD_PENDING_POLLS: Cell<usize> = const { Cell::new(0) };
    }

    /// Count of `Yield::poll` calls that returned `Pending` since the last call.
    /// Lets a scenario built around `sleep`/`sleep_until`
    /// assert it actually suspended at least once across a batch.
    ///
    /// Test-integrity check: a failure here means the scenario stopped exercising
    /// the code path it exists to cover.
    pub fn take_yield_pending_polls() -> usize {
        YIELD_PENDING_POLLS.with(|c| c.replace(0))
    }

    impl Future for Yield {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.yielded {
                Poll::Ready(())
            } else {
                self.yielded = true;
                YIELD_PENDING_POLLS.with(|c| c.set(c.get() + 1));
                LOGICAL_CLOCK
                    .with(|(_, nanos)| nanos.set(nanos.get() + LOGICAL_TICK.as_nanos() as u64));
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    /// No virtual clock to compare `_duration` against: each `Pending`
    /// poll of `future` has a small chance of simulating the deadline
    /// instead of waiting, so short futures rarely get cut off while
    /// long ones accumulate rising odds across a `pct`/`determinism` batch.
    /// Verified to fire at least once per batch by
    /// `worker::shuttle_tests::shuttle_background_task_drain_timeout_fires`,
    /// via `take_elapsed_fired`.
    pub fn timeout<F: Future + Unpin>(_duration: std::time::Duration, future: F) -> Timeout<F> {
        Timeout {
            future,
            pending_polls: 0,
        }
    }

    #[derive(Debug)]
    pub struct Elapsed(());

    pub struct Timeout<F> {
        future: F,
        pending_polls: u32,
    }

    // Per-pending-poll odds of simulating the deadline: mean 1/p = 50 pending
    // polls before firing. Placeholder value: picked so a short drain (a
    // handful of polls) is unlikely to trip it, never measured against
    // real polling counts. No scenario checks that property; only the
    // opposite one (eventual firing on a permanently-pending future) is
    // tested, and that guarantee comes from
    // `MAX_PENDING_POLLS_BEFORE_FORCED_ELAPSED` below, not from this value.
    const FIRE_PROBABILITY_PER_PENDING_POLL: f64 = 0.02;

    // Forces `Elapsed` after this many pending polls even without the dice
    // landing, so unbounded bad luck can't exceed shuttle's own
    // (process-wide, uncontrollable) step budget. ~8x the mean above, so it
    // almost never triggers in practice.
    const MAX_PENDING_POLLS_BEFORE_FORCED_ELAPSED: u32 = 400;

    // Accumulates across a whole `check_pct`/determinism batch (shuttle
    // resets its own state every iteration). Thread-local so a
    // concurrently-running `#[test]`, on its own OS thread, can't inflate
    // this one's count.
    std::thread_local! {
        static ELAPSED_FIRED: Cell<usize> = const { Cell::new(0) };
    }

    /// Count of `Timeout::poll` calls that resolved `Err(Elapsed)` since the
    /// last call. Lets a scenario built around `primitives::time::timeout`
    /// assert the deadline branch actually fired at least once across a
    /// batch.
    ///
    /// Test-integrity check: a failure here means the scenario stopped
    /// exercising the code path it exists to cover.
    pub fn take_elapsed_fired() -> usize {
        ELAPSED_FIRED.with(|c| c.replace(0))
    }

    impl<F: Future + Unpin> Future for Timeout<F> {
        type Output = Result<F::Output, Elapsed>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            use shuttle::rand::Rng;
            match Pin::new(&mut self.future).poll(cx) {
                Poll::Ready(v) => Poll::Ready(Ok(v)),
                Poll::Pending => {
                    self.pending_polls += 1;
                    if self.pending_polls >= MAX_PENDING_POLLS_BEFORE_FORCED_ELAPSED
                        || shuttle::rand::thread_rng().gen_bool(FIRE_PROBABILITY_PER_PENDING_POLL)
                    {
                        ELAPSED_FIRED.with(|c| c.set(c.get() + 1));
                        Poll::Ready(Err(Elapsed(())))
                    } else {
                        // Must self-wake: `future` may never wake anything
                        // on its own (e.g. `std::future::pending()`), and
                        // without this the dice above would only ever be
                        // rolled once.
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
            }
        }
    }
}

/// `tokio::select!` normally; `shuttle_tokio_impl_inner::select!` under
/// `--cfg shuttle`, which patches `select!`'s branch tie-break to draw from
/// `shuttle::rand::thread_rng()` instead of real OS entropy, so a replayed
/// schedule picks the same branch every time.
///
/// Doesn't cover `sleep`/`sleep_until`: its `Sleep` resolves immediately on
/// first poll instead of suspending, so `primitives::time` keeps its own
/// `Yield` for those.
#[cfg(all(shuttle, feature = "pipeline"))]
#[macro_export]
macro_rules! shuttle_select {
    ($($arms:tt)*) => {
        shuttle_tokio_impl_inner::select! { $($arms)* }
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

/// Real `tokio::runtime::Builder` normally; under `--cfg shuttle`,
/// `shuttle-tokio-impl-inner`'s stand-in, whose `block_on` is
/// `shuttle::future::block_on`: a genuine `Runtime` would capture
/// shuttle's one OS thread forever.
#[cfg(all(not(shuttle), feature = "pipeline"))]
pub mod runtime {
    pub use tokio::runtime::Builder;
}
#[cfg(all(shuttle, feature = "pipeline"))]
pub mod runtime {
    pub use shuttle_tokio_impl_inner::runtime::Builder;
}

/// Coroutine stack size for a shuttle scenario whose call depth SIGBUSes
/// on the bare-core 60KB default. Matches `shuttle-tokio`'s own default.
/// Use via `shuttle_test!`'s `stack_size = $bytes` modifier.
#[cfg(shuttle)]
pub const SHUTTLE_TOKIO_STACK_SIZE: usize = 0x000F_0000;

/// Shared by `shuttle_test!`'s `stack_size` arms so a custom `Config` doesn't
/// need `shuttle::check_pct`'s scheduler-construction internals reimplemented
/// at each arm.
#[cfg(shuttle)]
pub fn run_stack_bumped_pct<F: Fn() + Send + Sync + 'static>(
    f: F,
    depth: usize,
    num_iters: usize,
    config: shuttle::Config,
) {
    use shuttle::scheduler::PctScheduler;
    let scheduler = PctScheduler::new(depth, num_iters);
    shuttle::Runner::new(scheduler, config).run(f);
}

/// Same as [`run_stack_bumped_pct`], for `shuttle::check_uncontrolled_nondeterminism`.
#[cfg(shuttle)]
pub fn run_stack_bumped_determinism<F: Fn() + Send + Sync + 'static>(
    f: F,
    num_iters: usize,
    config: shuttle::Config,
) {
    use shuttle::scheduler::{RandomScheduler, UncontrolledNondeterminismCheckScheduler};
    let scheduler = UncontrolledNondeterminismCheckScheduler::new(RandomScheduler::new(num_iters));
    shuttle::Runner::new(scheduler, config).run(f);
}

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
/// - `verify_elapsed_triggered`: also asserts
///   `primitives::time::take_elapsed_fired() > 0`, so `primitives::time::timeout`'s
///   `Elapsed` branch can't silently stop firing across a batch.
/// - `stack_size = $bytes`: build `shuttle::Runner` directly with a
///   bumped coroutine stack, for a scenario whose call depth SIGBUSes on
///   the hardcoded 60KB default. Pass [`SHUTTLE_TOKIO_STACK_SIZE`].
/// - `verify_yield_triggered` (added after `stack_size = $bytes`): also
///   asserts `primitives::time::take_yield_pending_polls() > 0`, so a
///   `sleep`/`sleep_until` await can't silently stop suspending across a
///   batch.
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
    //
    // If also using `replay = $schedule`: capture it from `pct`'s failure
    // output, not `determinism`'s. `determinism`'s schedules come from
    // the same record-twice mechanism that SIGABRTs, so replaying one
    // could reproduce the abort instead of a catchable panic.
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
    // Same as the plain form, but also asserts
    // `primitives::time::take_elapsed_fired() > 0` across the whole batch, so
    // `primitives::time::timeout`'s `Elapsed` branch can't silently stop
    // firing without failing loudly. Checked inside the same
    // `pct`/`determinism` runs, not separate tests, to avoid exploring twice.
    (num_iters = $num_iters:expr, depth = $depth:expr, verify_elapsed_triggered; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            $(#[$attr])*
            fn $name() $body

            fn assert_elapsed_was_triggered() {
                assert!(
                    $crate::primitives::time::take_elapsed_fired() > 0,
                    "no run across {} iterations took primitives::time::timeout's Elapsed \
                     branch; this scenario is not exercising the drain-timeout race it exists \
                     to cover.",
                    $num_iters,
                );
            }

            #[test]
            fn pct() {
                $crate::primitives::time::take_elapsed_fired(); // drain any count left over from an earlier test
                shuttle::check_pct($name, $num_iters, $depth);
                assert_elapsed_was_triggered();
            }

            #[test]
            fn determinism() {
                $crate::primitives::time::take_elapsed_fired(); // drain any count left over from an earlier test
                shuttle::check_uncontrolled_nondeterminism($name, $num_iters);
                assert_elapsed_was_triggered();
            }
        }
    };
    // Same as the plain form, but builds `shuttle::Runner` directly with a
    // bumped `stack_size`, for a scenario whose call depth SIGBUSes on
    // the hardcoded 60KB default.
    (num_iters = $num_iters:expr, depth = $depth:expr, stack_size = $stack_size:expr; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            $(#[$attr])*
            fn $name() $body

            fn config() -> shuttle::Config {
                let mut config = shuttle::Config::new();
                config.stack_size = $stack_size;
                config
            }

            #[test]
            fn pct() {
                $crate::primitives::run_stack_bumped_pct($name, $depth, $num_iters, config());
            }

            #[test]
            fn determinism() {
                $crate::primitives::run_stack_bumped_determinism($name, $num_iters, config());
            }
        }
    };
    // Same as the `stack_size` form, but also asserts
    // `primitives::time::take_yield_pending_polls() > 0` across the whole
    // batch, for a scenario whose `sleep`/`sleep_until` await only suspends
    // on some schedules.
    (num_iters = $num_iters:expr, depth = $depth:expr, stack_size = $stack_size:expr, verify_yield_triggered; $(#[$attr:meta])* fn $name:ident() $body:block) => {
        mod $name {
            use super::*;

            $(#[$attr])*
            fn $name() $body

            fn config() -> shuttle::Config {
                let mut config = shuttle::Config::new();
                config.stack_size = $stack_size;
                config
            }

            fn assert_yield_was_triggered() {
                assert!(
                    $crate::primitives::time::take_yield_pending_polls() > 0,
                    "no run across {} iterations suspended on sleep/sleep_until; \
                     this scenario is not exercising the deadline path it exists \
                     to cover.",
                    $num_iters,
                );
            }

            #[test]
            fn pct() {
                $crate::primitives::time::take_yield_pending_polls(); // drain any count left over from an earlier test
                $crate::primitives::run_stack_bumped_pct($name, $depth, $num_iters, config());
                assert_yield_was_triggered();
            }

            #[test]
            fn determinism() {
                $crate::primitives::time::take_yield_pending_polls(); // drain any count left over from an earlier test
                $crate::primitives::run_stack_bumped_determinism($name, $num_iters, config());
                assert_yield_was_triggered();
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

    // Shuttle's threads are coroutines on one real OS thread, so this
    // stays visible to every spawned thread instead of being isolated per
    // logical thread. Swapping to `shuttle::thread` would silently stop fault injection
    // from reaching spawned threads. Pinned by `fs_fault_visible_across_threads`.
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

    // Accumulates across a whole `check_pct`/determinism batch (shuttle
    // resets its own state every iteration). Thread-local so a
    // concurrently-running `#[test]`, on its own OS thread, can't inflate
    // this one's count.
    std::thread_local! {
        static FAULTS_TRIGGERED: Cell<usize> = const { Cell::new(0) };
    }

    /// Count of `check()` calls that returned an error since the last call.
    /// Lets a scenario built around `set_fault` assert a fault actually
    /// fired at least once across a batch.
    ///
    /// Test-integrity check: a failure here means the scenario stopped
    /// exercising the code path it exists to cover.
    pub fn take_faults_triggered() -> usize {
        FAULTS_TRIGGERED.with(|c| c.replace(0))
    }

    fn check() -> io::Result<()> {
        let fail = match FAULT.with(|f| f.get()) {
            FaultPolicy::None => false,
            FaultPolicy::FailAll => true,
            FaultPolicy::FailProb(p) => shuttle::rand::thread_rng().gen_bool(p),
        };
        if fail {
            FAULTS_TRIGGERED.with(|c| c.set(c.get() + 1));
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
