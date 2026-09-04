//! The metrics.rs source through the path a user actually takes: registered on
//! a recorder with `with_metrics_rs`, driven by dial9's own flush thread.

use std::collections::HashMap;
use std::time::Duration;

use dial9_core::buffer::DiskBuffer;
use dial9_trace_format::types::FieldValueRef;
use dial9_utils::metrics_rs::{self, MetricsRsConfig, RecorderMetricsRsExt};

fn decode(dir: &std::path::Path) -> Vec<(String, HashMap<String, String>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.to_string_lossy().contains(".bin"))
        .collect();
    files.sort();

    let mut events = Vec::new();
    for path in files {
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let Some(mut decoder) = dial9_trace_format::decoder::Decoder::new(&data) else {
            continue;
        };
        let _ = decoder.for_each_event(|event| {
            if !event.name.starts_with("metricsrs:") {
                return;
            }
            let values = event
                .schema
                .fields()
                .iter()
                .zip(event.fields.iter())
                .map(|(def, value)| {
                    let rendered = match value {
                        FieldValueRef::Varint(v) => v.to_string(),
                        FieldValueRef::F64(v) => v.to_string(),
                        FieldValueRef::None => "<absent>".to_owned(),
                        other => format!("{other:?}"),
                    };
                    (def.name().to_owned(), rendered)
                })
                .collect();
            events.push((event.name.to_owned(), values));
        });
    }
    events
}

#[test]
fn the_flush_thread_samples_a_registered_source() {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("trace.bin");

    // A local recorder rather than the global one: `set_global_recorder` is
    // once per process, and test binaries run more than one test.
    let metrics_recorder = metrics_rs::recorder();
    let config = MetricsRsConfig::builder()
        .interval(Duration::from_millis(20))
        .build();

    let dial9_recorder = dial9_core::recorder::recorder(DiskBuffer::single_file(&trace).unwrap())
        .with_metrics_rs(metrics_recorder.clone(), config)
        .build();
    dial9_recorder.enable();

    metrics::with_local_recorder(&metrics_recorder, || {
        metrics::counter!("requests_total").increment(7);
        metrics::gauge!("pool_available").set(2.0);
    });

    // Shutdown also flushes sources, so the count is what distinguishes
    // periodic sampling from one final flush.
    std::thread::sleep(Duration::from_millis(300));
    dial9_recorder.graceful_shutdown(Duration::from_secs(5));

    let events = decode(dir.path());
    let counter: Vec<_> = events
        .iter()
        .filter(|(name, _)| name == "metricsrs:requests_total")
        .collect();
    assert!(
        counter.len() > 2,
        "the flush thread should have sampled repeatedly, got {} event(s): {events:#?}",
        counter.len()
    );
    // The counter never moves again, so every sample carries the same total.
    for (_, values) in &counter {
        assert_eq!(values["value"], "7");
    }

    let gauge: Vec<_> = events
        .iter()
        .filter(|(name, _)| name == "metricsrs:pool_available")
        .collect();
    assert!(!gauge.is_empty(), "{events:#?}");
    assert_eq!(gauge[0].1["value"], "2");
}

#[test]
fn a_paused_recorder_records_nothing() {
    // `build()` starts recording; `paused()` holds it off. The flush loop
    // skips `flush_sources` while paused, so no readout is ever taken.
    let dir = tempfile::tempdir().unwrap();
    let metrics_recorder = metrics_rs::recorder();

    let dial9_recorder = dial9_core::recorder::recorder(
        DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap(),
    )
    .with_metrics_rs(
        metrics_recorder.clone(),
        MetricsRsConfig::builder().interval(Duration::ZERO).build(),
    )
    .paused()
    .build();

    metrics::with_local_recorder(&metrics_recorder, || {
        metrics::counter!("requests_total").increment(1);
    });
    std::thread::sleep(Duration::from_millis(50));
    dial9_recorder.graceful_shutdown(Duration::from_secs(5));

    assert!(decode(dir.path()).is_empty());
}
