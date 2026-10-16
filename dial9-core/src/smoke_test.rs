//! Statuses the smoke test reports. The smoke test and its report live in the
//! `dial9` crate (`dial9::smoke_test`), which re-exports these.

use crate::handle::Dial9Handle;
use std::fmt;

/// Outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckStatus {
    /// The check passed.
    Ok,
    /// The check doesn't apply. Not a failure.
    #[non_exhaustive]
    Disabled {
        /// Why the check doesn't apply.
        reason: DisabledReason,
        /// Human-readable explanation.
        detail: String,
    },
    /// The check found a problem.
    #[non_exhaustive]
    Failed {
        /// Why the check failed.
        reason: FailedReason,
        /// Human-readable explanation.
        detail: String,
    },
}

/// Why a check is [`Disabled`](CheckStatus::Disabled).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DisabledReason {
    /// Recording is paused.
    RecordingPaused,
}

/// Why a check [`Failed`](CheckStatus::Failed).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailedReason {
    /// The handle isn't connected to a recorder.
    RecorderNotConnected,
    /// The recorder has shut down.
    RecorderStopped,
    /// Recording was permanently disabled by an initialization failure.
    RecorderFailed,
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Check {
    /// The recording check, reported by `dial9`'s
    /// `SmokeTestReport::recording_status`.
    Recording,
}

impl Check {
    /// Stable name for logs and metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Recording => "recording",
        }
    }
}

/// The recording check behind `dial9`'s `SmokeTestReport::recording_status`.
// Called only by `dial9`'s smoke test: kept out of the semver promise.
#[doc(hidden)]
pub fn check_recording(handle: &Dial9Handle) -> CheckStatus {
    use crate::shared_state::State;

    let Some(shared) = handle.shared() else {
        return CheckStatus::failed(
            FailedReason::RecorderNotConnected,
            "handle is not connected to a recorder",
        );
    };
    match shared.recording_state() {
        State::Enabled => CheckStatus::Ok,
        State::Disabled => {
            CheckStatus::disabled(DisabledReason::RecordingPaused, "recording is paused")
        }
        State::Stopped => {
            CheckStatus::failed(FailedReason::RecorderStopped, "recorder has shut down")
        }
        #[cfg(feature = "pipeline")]
        State::Failed => CheckStatus::failed(
            FailedReason::RecorderFailed,
            "recording was permanently disabled by an initialization failure",
        ),
    }
}

#[cfg(all(test, feature = "pipeline", not(shuttle)))]
mod tests {
    use super::*;
    use crate::primitives::sync::{Arc, mpsc};
    use crate::shared_state::SharedState;

    #[test]
    fn permanently_failed_recorder_is_not_a_healthy_pause() {
        let shared = Arc::new(SharedState::new(0));
        shared.enable();
        let (tx, _rx) = mpsc::sync_channel(1);
        let handle = Dial9Handle::enabled(shared.clone(), tx);
        shared.disable_permanently();

        assert!(matches!(
            check_recording(&handle),
            CheckStatus::Failed {
                reason: FailedReason::RecorderFailed,
                ..
            }
        ));
        handle.enable();
        assert!(matches!(
            check_recording(&handle),
            CheckStatus::Failed {
                reason: FailedReason::RecorderFailed,
                ..
            }
        ));
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    use crate::primitives::sync::{Arc, mpsc};
    use crate::primitives::thread;
    use crate::shared_state::SharedState;

    crate::shuttle_test! {
        default;
        fn shuttle_recording_check_races_shutdown() {
            let shared = Arc::new(SharedState::new(0));
            shared.enable();
            let (tx, _rx) = mpsc::sync_channel(1);
            let handle = Dial9Handle::enabled(shared.clone(), tx);

            let shutdown = thread::spawn(move || shared.mark_stopped());
            let status = check_recording(&handle);
            assert!(
                matches!(
                    status,
                    CheckStatus::Ok
                        | CheckStatus::Failed {
                            reason: FailedReason::RecorderStopped,
                            ..
                        }
                ),
                "an enabled recorder was never paused: {status:?}"
            );

            shutdown.join().unwrap();
            assert!(matches!(
                check_recording(&handle),
                CheckStatus::Failed {
                    reason: FailedReason::RecorderStopped,
                    ..
                }
            ));
        }
    }
}
