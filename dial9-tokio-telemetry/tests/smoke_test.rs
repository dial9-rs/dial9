//! Integration tests for `SmokeTester`'s recording check.

use dial9_core::handle::Dial9Handle;
use dial9_tokio_telemetry::telemetry::smoke_test::{Check, CheckStatus, DisabledReason};
use dial9_tokio_telemetry::telemetry::{Dial9HandleTokioExt, MemoryBuffer, recorder};
use std::time::Duration;

fn failed_with(status: &CheckStatus, text: &str) -> bool {
    matches!(status, CheckStatus::Failed { detail, .. } if detail.contains(text))
}

#[tokio::test]
async fn live_recorder_is_healthy() {
    let rec = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
    let report = rec.handle().smoke_tester().build().run().await;
    assert_eq!(report.recording(), &CheckStatus::Ok, "{report}");
    assert!(report.is_healthy());
    rec.graceful_shutdown(Duration::ZERO);
}

#[tokio::test]
async fn disconnected_handle_fails_recording() {
    let report = Dial9Handle::disabled().smoke_tester().build().run().await;
    assert!(failed_with(report.recording(), "not connected"), "{report}");
    assert!(!report.is_healthy());
}

#[tokio::test]
async fn stopped_recorder_fails_recording() {
    let rec = recorder(MemoryBuffer::new(1 << 20).unwrap()).build();
    let handle = rec.handle().clone();
    rec.graceful_shutdown(Duration::ZERO);
    let report = handle.smoke_tester().build().run().await;
    assert!(failed_with(report.recording(), "shut down"), "{report}");
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
            report.recording(),
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
    assert_eq!(
        report.to_string(),
        "recording: FAILED: handle is not connected to a recorder\n"
    );
}

/// `run()` can be spawned onto a multi-threaded runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn run_can_be_spawned() {
    let tester = Dial9Handle::disabled().smoke_tester().build();
    let report = tokio::spawn(async move { tester.run().await })
        .await
        .unwrap();
    assert!(!report.is_healthy());
}
