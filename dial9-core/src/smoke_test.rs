//! Statuses the smoke test reports. The tester and its report live in the
//! `dial9` crate (`dial9::smoke_test`), which re-exports these.

use crate::handle::Dial9Handle;
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
        /// Why the check doesn't apply.
        reason: DisabledReason,
        /// Human-readable explanation.
        detail: String,
    },
    /// Should work, doesn't.
    #[non_exhaustive]
    Failed {
        /// Why the check failed.
        reason: FailedReason,
        /// Human-readable explanation.
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

/// Why a check [`Failed`](CheckStatus::Failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailedReason {
    /// The handle isn't connected to a recorder.
    RecorderNotConnected,
    /// The recorder has shut down.
    RecorderStopped,
}

impl CheckStatus {
    fn failed(reason: FailedReason, detail: impl Into<String>) -> Self {
        Self::Failed {
            reason,
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
            Self::Failed { detail, .. } => write!(f, "FAILED: {detail}"),
        }
    }
}

/// One check in the smoke test report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Check {
    /// Whether the handle is connected to a recorder that hasn't shut down
    /// and isn't paused.
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

/// Whether `handle` is connected to a recorder that hasn't shut down and isn't
/// paused. Doesn't check that the recorder's threads are alive.
// Called only by `dial9`'s smoke tester: kept out of the semver promise.
#[doc(hidden)]
pub fn check_recording(handle: &Dial9Handle) -> CheckStatus {
    if !handle.is_connected() {
        CheckStatus::failed(
            FailedReason::RecorderNotConnected,
            "handle is not connected to a recorder",
        )
    } else if handle.is_stopped() {
        CheckStatus::failed(FailedReason::RecorderStopped, "recorder has shut down")
    } else if !handle.is_enabled() {
        CheckStatus::disabled(DisabledReason::RecordingPaused, "recording is paused")
    } else {
        CheckStatus::Ok
    }
}
