//! Integration tests for `SmokeTester`'s recording check.

use dial9::smoke_test::{Check, CheckStatus, DisabledReason, FailedReason, SmokeTesterExt};
use dial9::{Dial9Handle, MemoryBuffer, recorder};
use std::time::Duration;

fn failed_with(status: &CheckStatus, expected: FailedReason) -> bool {
    matches!(status, CheckStatus::Failed { reason, .. } if *reason == expected)
}

#[tokio::test]
async fn live_recorder_is_healthy() {
    let rec = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
    let report = rec.handle().smoke_tester().build().run().await;
    assert_eq!(report.recording_status(), &CheckStatus::Ok, "{report}");
    assert!(report.is_healthy());
    rec.graceful_shutdown(Duration::ZERO);
}

#[tokio::test]
async fn disconnected_handle_fails_recording() {
    let report = Dial9Handle::disabled().smoke_tester().build().run().await;
    assert!(
        failed_with(
            report.recording_status(),
            FailedReason::RecorderNotConnected
        ),
        "{report}"
    );
    assert!(!report.is_healthy());
}

#[tokio::test]
async fn stopped_recorder_fails_recording() {
    let rec = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
    let handle = rec.handle().clone();
    rec.graceful_shutdown(Duration::ZERO);
    let report = handle.smoke_tester().build().run().await;
    assert!(
        failed_with(report.recording_status(), FailedReason::RecorderStopped),
        "{report}"
    );
    assert!(!report.is_healthy());
}

/// A pause is not a failure.
#[tokio::test]
async fn paused_recorder_is_disabled_not_failed() {
    let rec = recorder(MemoryBuffer::new(1 << 20).unwrap())
        .paused()
        .build();
    let report = rec.handle().smoke_tester().build().run().await;
    assert!(
        matches!(
            report.recording_status(),
            CheckStatus::Disabled {
                reason: DisabledReason::RecordingPaused,
                ..
            }
        ),
        "{report}"
    );
    assert!(report.is_healthy());
    rec.graceful_shutdown(Duration::ZERO);
}

#[tokio::test]
async fn report_lists_each_check_by_name() {
    let report = Dial9Handle::disabled().smoke_tester().build().run().await;
    let checks: Vec<_> = report.checks().map(|(check, _)| check).collect();
    assert_eq!(checks, [Check::Recording]);
    assert_eq!(report.to_string().lines().count(), checks.len(), "{report}");
    assert!(
        report
            .to_string()
            .lines()
            .any(|line| line == "recording: FAILED: handle is not connected to a recorder"),
        "{report}"
    );
}
