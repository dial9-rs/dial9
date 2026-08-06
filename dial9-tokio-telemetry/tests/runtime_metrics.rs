#![cfg(feature = "analysis")]
//! Verify that the flush thread emits per-runtime scheduler metrics
//! (`RuntimeMetricsEvent`), one sample per attached runtime, each tagged with
//! its runtime name — rather than a single queue-depth sample summed across
//! every runtime (the superseded `QueueSampleEvent`).

mod common;

use common::decode_file;
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{DiskBuffer, TokioAttachOptions, recorder};
use std::collections::HashSet;
use std::time::Duration;

#[test]
fn flush_emits_per_runtime_metrics_tagged_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();

    let recorder = recorder(writer).build();

    // Primary (unnamed) runtime — surfaces as the empty runtime name.
    let main_rt = common::attach(
        &recorder,
        2,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .build(),
    );
    // Secondary named runtime, sharing the same trace session.
    let io_rt = common::attach(
        &recorder,
        2,
        TokioAttachOptions::builder()
            .runtime_name("io")
            .task_tracking_enabled(true)
            .build(),
    );

    // Drive a little work on each and leave both alive long enough that the
    // flush thread (10ms sample interval) runs several sample cycles.
    main_rt.block_on(async {
        for _ in 0..20 {
            tokio::spawn(async { tokio::task::yield_now().await });
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    });
    io_rt.block_on(async {
        for _ in 0..10 {
            tokio::spawn(async { tokio::task::yield_now().await });
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    });

    drop(main_rt);
    drop(io_rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    let files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
        .collect();
    let events: Vec<Dial9Event> = files.iter().flat_map(|f| decode_file(f)).collect();

    // The recorder no longer emits the legacy summed sample.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Dial9Event::QueueSampleEvent(_))),
        "recorder must not emit the superseded QueueSampleEvent"
    );

    let runtime_names: HashSet<&str> = events
        .iter()
        .filter_map(|e| match e {
            Dial9Event::RuntimeMetricsEvent(m) => Some(m.runtime_name.as_str()),
            _ => None,
        })
        .collect();

    // The unnamed primary runtime reports as the empty name; "io" is tagged.
    assert!(
        runtime_names.contains(""),
        "expected a RuntimeMetricsEvent for the unnamed primary runtime; saw {runtime_names:?}"
    );
    assert!(
        runtime_names.contains("io"),
        "expected a RuntimeMetricsEvent tagged 'io'; saw {runtime_names:?}"
    );
}
