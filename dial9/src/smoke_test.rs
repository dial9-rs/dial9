//! Pre-flight check of dial9 telemetry.
//!
//! Build a [`SmokeTester`] with `handle.smoke_tester()` and call
//! [`run`](SmokeTester::run) where the check belongs, such as a startup probe
//! or a deploy gate.
//!
//! ```no_run
//! # async fn f(handle: dial9::Dial9Handle) {
//! use dial9::smoke_test::SmokeTesterExt;
//!
//! let tester = handle.smoke_tester().build();
//! let report = tester.run().await;
//! if !report.is_healthy() {
//!     eprintln!("dial9 smoke test failed:\n{report}");
//! }
//! # }
//! ```

use dial9_core::handle::Dial9Handle;
use std::fmt;
use std::marker::PhantomData;

pub use dial9_core::smoke_test::{Check, CheckStatus, DisabledReason};

/// Result of [`SmokeTester::run`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SmokeTestReport {
    recording: CheckStatus,
}

impl SmokeTestReport {
    /// Whether the handle is connected to a recorder that hasn't shut down
    /// and isn't paused. Doesn't check that the recorder's threads are alive.
    pub fn recording(&self) -> &CheckStatus {
        &self.recording
    }

    /// Every check with its identifier.
    pub fn checks(&self) -> impl Iterator<Item = (Check, &CheckStatus)> {
        [(Check::Recording, &self.recording)].into_iter()
    }

    /// `false` if any check [`Failed`](CheckStatus::Failed).
    pub fn is_healthy(&self) -> bool {
        !self
            .checks()
            .any(|(_, status)| matches!(status, CheckStatus::Failed { .. }))
    }
}

impl fmt::Display for SmokeTestReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (check, status) in self.checks() {
            writeln!(f, "{}: {status}", check.as_str())?;
        }
        Ok(())
    }
}

/// Runs the smoke test against the recorder behind a handle.
pub struct SmokeTester {
    handle: Dial9Handle,
    // Gives the type now the auto traits the result cache will give it (not
    // `RefUnwindSafe`), so adding the cache isn't a semver break.
    _cache: PhantomData<tokio::sync::Mutex<()>>,
}

impl fmt::Debug for SmokeTester {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmokeTester").finish_non_exhaustive()
    }
}

#[bon::bon]
impl SmokeTester {
    // `new` stays private: later settings become its parameters, so only the
    // builder is public API.
    #[builder(
        start_fn(name = builder_owned, vis = ""),
        finish_fn(name = build, vis = "pub"),
        builder_type(name = SmokeTesterBuilder, vis = "pub")
    )]
    fn new(#[builder(start_fn)] handle: Dial9Handle) -> Self {
        Self {
            handle,
            _cache: PhantomData,
        }
    }
}

impl SmokeTester {
    /// Tester for the recorder behind `handle`.
    pub fn builder(handle: &Dial9Handle) -> SmokeTesterBuilder {
        Self::builder_owned(handle.clone())
    }

    /// Run every check. Works on any Tokio runtime.
    pub async fn run(&self) -> SmokeTestReport {
        SmokeTestReport {
            recording: dial9_core::smoke_test::check_recording(&self.handle),
        }
    }
}

/// Adds [`smoke_tester`](Self::smoke_tester) to [`Dial9Handle`].
pub trait SmokeTesterExt: smoke_tester_ext_sealed::Sealed {
    /// Tester for the recorder behind this handle. Same as
    /// [`SmokeTester::builder`].
    fn smoke_tester(&self) -> SmokeTesterBuilder;
}

mod smoke_tester_ext_sealed {
    pub trait Sealed {}
    impl Sealed for dial9_core::handle::Dial9Handle {}
}

impl SmokeTesterExt for Dial9Handle {
    fn smoke_tester(&self) -> SmokeTesterBuilder {
        SmokeTester::builder(self)
    }
}

// `run()` must stay spawnable.
const _: fn(&'static SmokeTester) = |t| {
    fn assert_send<X: Send>(_: X) {}
    assert_send(t.run());
};
