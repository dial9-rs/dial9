//! Pre-flight check of dial9 telemetry.
//!
//! Build a [`SmokeTest`] with `handle.smoke_test()` and call
//! [`validate`](SmokeTest::validate) where the check belongs, such as a startup probe
//! or a deploy gate.
//!
//! ```no_run
//! # async fn f(handle: dial9::Dial9Handle) {
//! use dial9::smoke_test::SmokeTestExt;
//!
//! let smoke_test = handle.smoke_test().build();
//! let report = smoke_test.validate().await;
//! for (check, status) in report.failed_checks() {
//!     eprintln!("dial9 smoke test failed for {}: {status}", check.as_str());
//! }
//! # }
//! ```

use dial9_core::handle::Dial9Handle;
use std::fmt;
use std::marker::PhantomData;

pub use dial9_core::smoke_test::{Check, CheckStatus, DisabledReason, FailedReason};

/// Result of [`SmokeTest::validate`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SmokeTestReport {
    recording: CheckStatus,
}

impl SmokeTestReport {
    /// Status of recording on the handle:
    /// - `Ok`: connected to a running recorder that isn't paused.
    /// - `Disabled`: recording is paused.
    /// - `Failed`: not connected to a recorder, permanently disabled by an
    ///   initialization failure, or shut down.
    ///
    /// Doesn't check that the recorder's threads are alive.
    pub fn recording_status(&self) -> &CheckStatus {
        &self.recording
    }

    /// Every check with its identifier.
    pub fn checks(&self) -> impl Iterator<Item = (Check, &CheckStatus)> {
        [(Check::Recording, &self.recording)].into_iter()
    }

    /// Every failed check with its identifier and status.
    ///
    /// Disabled checks are excluded because they are not failures.
    pub fn failed_checks(&self) -> impl Iterator<Item = (Check, &CheckStatus)> {
        self.checks()
            .filter(|(_, status)| matches!(status, CheckStatus::Failed { .. }))
    }

    /// `false` if any check [`Failed`](CheckStatus::Failed).
    pub fn is_healthy(&self) -> bool {
        self.failed_checks().next().is_none()
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
pub struct SmokeTest {
    handle: Dial9Handle,
    // Matches the auto traits a later result cache will add (not
    // `RefUnwindSafe`), so adding it isn't a semver break.
    _cache: PhantomData<tokio::sync::Mutex<()>>,
}

impl fmt::Debug for SmokeTest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmokeTest").finish_non_exhaustive()
    }
}

#[bon::bon]
impl SmokeTest {
    // `new` and the start function stay private: `handle.smoke_test()` is
    // the one way in, and later settings become `new`'s parameters.
    #[builder(
        start_fn(name = builder, vis = ""),
        finish_fn(name = build, vis = "pub"),
        builder_type(name = SmokeTestBuilder, vis = "pub")
    )]
    fn new(#[builder(start_fn)] handle: Dial9Handle) -> Self {
        Self {
            handle,
            _cache: PhantomData,
        }
    }
}

impl SmokeTest {
    /// Run every check. Works on any Tokio runtime.
    pub async fn validate(&self) -> SmokeTestReport {
        SmokeTestReport {
            recording: dial9_core::smoke_test::check_recording(&self.handle),
        }
    }
}

/// Adds [`smoke_test`](Self::smoke_test) to [`Dial9Handle`].
pub trait SmokeTestExt: smoke_test_ext_sealed::Sealed {
    /// Starts building a [`SmokeTest`] for the recorder behind this handle.
    fn smoke_test(&self) -> SmokeTestBuilder;
}

mod smoke_test_ext_sealed {
    pub trait Sealed {}
    impl Sealed for dial9_core::handle::Dial9Handle {}
}

impl SmokeTestExt for Dial9Handle {
    fn smoke_test(&self) -> SmokeTestBuilder {
        SmokeTest::builder(self.clone())
    }
}

// `validate()` must stay spawnable.
const _: fn(&'static SmokeTest) = |t| {
    fn assert_send<X: Send>(_: X) {}
    assert_send(t.validate());
};
