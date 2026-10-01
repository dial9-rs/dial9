//! Pre-flight check that telemetry is live.
//!
//! Build a [`SmokeTester`] with `handle.smoke_tester()` and call
//! [`run`](SmokeTester::run) where the check belongs, such as a startup probe
//! or a deploy gate.
//!
//! ```no_run
//! # async fn f(handle: dial9_core::handle::Dial9Handle) {
//! use dial9_tokio_telemetry::telemetry::Dial9HandleTokioExt;
//!
//! let tester = handle.smoke_tester().build();
//! let report = tester.run().await;
//! if !report.is_healthy() {
//!     tracing::error!("dial9 smoke test failed:\n{report}");
//! }
//! # }
//! ```

use dial9_core::handle::Dial9Handle;
use std::fmt;

/// Outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckStatus {
    /// Working.
    Ok,
    /// The check doesn't apply. Not a failure.
    #[non_exhaustive]
    Disabled {
        /// Why it doesn't apply.
        reason: DisabledReason,
        /// The same, for people.
        detail: String,
    },
    /// Should work, doesn't.
    #[non_exhaustive]
    Failed {
        /// Why.
        detail: String,
    },
}

/// Why a check is [`Disabled`](CheckStatus::Disabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DisabledReason {
    /// Recording is paused.
    RecordingPaused,
}

impl CheckStatus {
    fn failed(detail: impl Into<String>) -> Self {
        Self::Failed {
            detail: detail.into(),
        }
    }

    fn disabled(reason: DisabledReason, detail: impl Into<String>) -> Self {
        Self::Disabled {
            reason,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Disabled { detail, .. } => write!(f, "disabled: {detail}"),
            Self::Failed { detail } => write!(f, "FAILED: {detail}"),
        }
    }
}

/// One check in a [`SmokeTestReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Check {
    /// [`SmokeTestReport::recording`].
    Recording,
}

impl Check {
    /// Stable name for logs and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
        }
    }
}

/// Result of [`SmokeTester::run`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SmokeTestReport {
    recording: CheckStatus,
}

impl SmokeTestReport {
    /// Whether the handle is connected to a live, unpaused recorder.
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
}

impl fmt::Debug for SmokeTester {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmokeTester").finish_non_exhaustive()
    }
}

#[bon::bon]
impl SmokeTester {
    /// Tester for the recorder behind `handle`, reached as
    /// `handle.smoke_tester()` through `Dial9HandleTokioExt`.
    // `new` stays private: later settings become its parameters, so only the
    // builder is public API.
    #[builder(
        start_fn(name = builder, vis = "pub(crate)"),
        finish_fn(name = build, vis = "pub"),
        builder_type(name = SmokeTesterBuilder, vis = "pub")
    )]
    fn new(#[builder(start_fn)] handle: Dial9Handle) -> Self {
        Self { handle }
    }
}

// `run()` must stay spawnable.
const _: fn(&'static SmokeTester) = |t| {
    fn assert_send<X: Send>(_: X) {}
    assert_send(t.run());
};

impl SmokeTester {
    /// Run every check.
    pub async fn run(&self) -> SmokeTestReport {
        SmokeTestReport {
            recording: check_recording(&self.handle),
        }
    }
}

fn check_recording(handle: &Dial9Handle) -> CheckStatus {
    if !handle.is_connected() {
        CheckStatus::failed("handle is not connected to a recorder")
    } else if handle.is_stopped() {
        CheckStatus::failed("recorder has shut down")
    } else if !handle.is_enabled() {
        CheckStatus::disabled(DisabledReason::RecordingPaused, "recording is paused")
    } else {
        CheckStatus::Ok
    }
}
