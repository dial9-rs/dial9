//! End-to-end tests for the encode path: captured metrics through a real
//! recorder into a sealed trace, decoded back for schema shape, annotations,
//! and values.

use std::collections::HashMap;
use std::time::Duration;

use dial9_core::buffer::DiskBuffer;
use dial9_trace_format::types::FieldValueRef;
use metrique_metricsrs::{MetricRecorder, ParametricRecorder};

use super::capture::{Percentile, capture};
use super::encode::{Schemas, write};

/// One decoded event: schema name, field order, values, and annotations.
#[derive(Debug)]
struct DecodedEvent {
    name: String,
    field_names: Vec<String>,
    values: HashMap<String, String>,
    annotations: HashMap<String, HashMap<String, String>>,
}

fn render(value: &FieldValueRef<'_>) -> String {
    match value {
        FieldValueRef::Varint(v) => v.to_string(),
        FieldValueRef::F64(v) => v.to_string(),
        FieldValueRef::None => "<absent>".to_owned(),
        other => format!("{other:?}"),
    }
}

/// Record through a local metrics.rs recorder, sample it `intervals` times,
/// and decode every `metricsrs:` event back out of the sealed trace.
fn round_trip(intervals: usize, mut record: impl FnMut(usize)) -> Vec<DecodedEvent> {
    let percentiles: Vec<_> = [0.5, 0.99].into_iter().map(Percentile::new).collect();
    let metrics_recorder: MetricRecorder<dyn metrics::Recorder> = MetricRecorder::new();

    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();
    let dial9_recorder = dial9_core::recorder::recorder(writer).build();
    dial9_recorder.enable();
    let handle = dial9_recorder.handle();

    let mut schemas = Schemas::default();
    let mut samples = Vec::new();
    let mut values = Vec::new();
    for interval in 0..intervals {
        metrics_recorder.with_local_recorder(|| record(interval));
        capture(&metrics_recorder.readout(), &percentiles, &mut samples);
        handle.with_encoder(|encoder| {
            for sample in &samples {
                write(
                    encoder,
                    &mut schemas,
                    1_000_000 * (interval as u64 + 1),
                    sample,
                    &mut values,
                );
            }
        });
    }
    dial9_recorder.graceful_shutdown(Duration::from_secs(5));

    decode(dir.path())
}

fn decode(dir: &std::path::Path) -> Vec<DecodedEvent> {
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
        // Ignore a trailing decode error from a partially-written .active tail.
        let _ = decoder.for_each_event(|event| {
            if !event.name.starts_with("metricsrs:") {
                return;
            }
            let mut field_names = Vec::new();
            let mut values = HashMap::new();
            let mut annotations: HashMap<String, HashMap<String, String>> = HashMap::new();
            for (index, (def, value)) in event
                .schema
                .fields()
                .iter()
                .zip(event.fields.iter())
                .enumerate()
            {
                field_names.push(def.name().to_owned());
                values.insert(def.name().to_owned(), render(value));
                for annotation in event.schema.annotations() {
                    if annotation.field_index() as usize == index {
                        annotations
                            .entry(def.name().to_owned())
                            .or_default()
                            .insert(annotation.key().to_owned(), annotation.value().to_owned());
                    }
                }
            }
            events.push(DecodedEvent {
                name: event.name.to_owned(),
                field_names,
                values,
                annotations,
            });
        });
    }
    events
}

fn named<'a>(events: &'a [DecodedEvent], name: &str) -> Vec<&'a DecodedEvent> {
    events.iter().filter(|event| event.name == name).collect()
}

#[test]
fn every_metric_gets_its_own_prefixed_schema() {
    let events = round_trip(1, |_| {
        metrics::counter!("requests_total").increment(5);
        metrics::gauge!("pool_available").set(3.0);
    });

    let mut names: Vec<_> = events.iter().map(|event| event.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["metricsrs:pool_available", "metricsrs:requests_total"]
    );
}

#[test]
fn counter_and_gauge_carry_their_kind_and_value() {
    let events = round_trip(1, |_| {
        metrics::counter!("requests_total").increment(5);
        metrics::gauge!("pool_available").set(3.5);
    });

    let counter = named(&events, "metricsrs:requests_total")[0];
    assert_eq!(counter.field_names, vec!["value"]);
    assert_eq!(counter.values["value"], "5");
    assert_eq!(counter.annotations["value"]["kind"], "counter");

    let gauge = named(&events, "metricsrs:pool_available")[0];
    assert_eq!(gauge.values["value"], "3.5");
    assert_eq!(gauge.annotations["value"]["kind"], "gauge");
}

#[test]
fn histogram_lands_as_one_event_with_all_its_fields() {
    let events = round_trip(1, |_| {
        for value in [10.0, 20.0, 30.0, 40.0] {
            metrics::histogram!("query_ms").record(value);
        }
    });

    let histogram = named(&events, "metricsrs:query_ms")[0];
    assert_eq!(histogram.field_names, vec!["count", "sum", "p50", "p99"]);
    assert_eq!(histogram.values["count"], "4");
    // count accumulates, percentiles do not.
    assert_eq!(histogram.annotations["count"]["kind"], "counter");
    assert_eq!(histogram.annotations["p99"]["kind"], "gauge");
}

#[test]
fn described_units_reach_the_schema_annotations() {
    let events = round_trip(1, |_| {
        metrics::describe_histogram!("query_ms", metrics::Unit::Milliseconds, "");
        metrics::histogram!("query_ms").record(12.0);
        metrics::counter!("undescribed").increment(1);
    });

    let histogram = named(&events, "metricsrs:query_ms")[0];
    assert_eq!(histogram.annotations["p50"]["unit"], "ms");
    assert_eq!(histogram.annotations["count"]["unit"], "count");

    let plain = named(&events, "metricsrs:undescribed")[0];
    assert!(
        !plain.annotations["value"].contains_key("unit"),
        "{:?}",
        plain.annotations
    );
}

#[test]
fn a_metric_appearing_late_does_not_disturb_the_others() {
    // The reason for per-metric schemas: each metric registers its own shape
    // when it first appears.
    let events = round_trip(3, |interval| {
        metrics::counter!("early").increment(1);
        if interval == 2 {
            metrics::counter!("late").increment(7);
        }
    });

    assert_eq!(named(&events, "metricsrs:early").len(), 3);
    let late = named(&events, "metricsrs:late");
    assert_eq!(late.len(), 1);
    assert_eq!(late[0].values["value"], "7");
}

#[test]
fn repeated_intervals_reuse_one_schema() {
    let events = round_trip(4, |_| {
        metrics::counter!("requests_total").increment(2);
    });

    let counter = named(&events, "metricsrs:requests_total");
    assert_eq!(counter.len(), 4);
    // Each interval reports its own delta; the source turns these into a
    // running total, which is why the wire value is a counter kind.
    for event in counter {
        assert_eq!(event.values["value"], "2");
    }
}

#[test]
fn labelled_metrics_keep_their_labels_in_the_schema_name() {
    let events = round_trip(1, |_| {
        metrics::counter!("errors", "kind" => "timeout").increment(1);
    });

    assert_eq!(events[0].name, "metricsrs:errors{kind=timeout}");
}

#[test]
fn schemas_are_built_once_per_metric_not_per_interval() {
    // Reuse matters beyond allocation: the encoder's fast path keys on the
    // schema's allocation identity.
    let percentiles: Vec<_> = [0.5].into_iter().map(Percentile::new).collect();
    let metrics_recorder: MetricRecorder<dyn metrics::Recorder> = MetricRecorder::new();

    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();
    let dial9_recorder = dial9_core::recorder::recorder(writer).build();
    dial9_recorder.enable();
    let handle = dial9_recorder.handle();

    let mut schemas = Schemas::default();
    let mut samples = Vec::new();
    let mut values = Vec::new();
    for interval in 0..5u64 {
        metrics_recorder.with_local_recorder(|| {
            metrics::counter!("a").increment(1);
            metrics::counter!("b").increment(1);
        });
        capture(&metrics_recorder.readout(), &percentiles, &mut samples);
        handle.with_encoder(|encoder| {
            for sample in &samples {
                assert!(write(
                    encoder,
                    &mut schemas,
                    1_000_000 * (interval + 1),
                    sample,
                    &mut values
                ));
            }
        });
        assert_eq!(schemas.len(), 2, "after interval {interval}");
    }
    dial9_recorder.graceful_shutdown(Duration::from_secs(5));
}
