#![cfg(feature = "analysis")]
//! Regression: core-side `.segment_metadata(..)` survives `attach_runtime(..)`.

mod common;

use common::decode_file;
use dial9_tokio_telemetry::telemetry::analysis_events::Dial9Event;
use dial9_tokio_telemetry::telemetry::{DiskBuffer, RecorderTokioExt, recorder};
use std::time::Duration;

#[test]
fn core_segment_metadata_survives_attach_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();

    let recorder = recorder(writer)
        .segment_metadata([("service".to_string(), "checkout".to_string())])
        .build();
    let (recorder, rt) = recorder
        .attach_runtime(|t| {
            t.worker_threads(1);
        })
        .expect("build tokio runtime");

    rt.block_on(async {
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
    drop(rt);
    recorder.graceful_shutdown(Duration::from_secs(5));

    let mut found = false;
    let files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
        .collect();

    for file in &files {
        let events: Vec<Dial9Event> = decode_file(file);
        for event in &events {
            if let Dial9Event::SegmentMetadataEvent(m) = event {
                found |= m.entries.get("service").map(String::as_str) == Some("checkout");
            }
        }
    }

    assert!(
        found,
        "core-side segment_metadata must survive attach_runtime(), files: {files:?}"
    );
}
