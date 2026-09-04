//! Tests for the cross-interval behaviour: the interval gate, running counter
//! totals, carrying quiet metrics, and the metric cap.

use std::collections::HashMap;
use std::time::Duration;

use dial9_core::buffer::DiskBuffer;
use dial9_trace_format::types::FieldValueRef;

use super::source::{MetricsRsConfig, MetricsRsSource, recorder};

/// One decoded event, in trace order.
#[derive(Debug, Clone)]
struct Event {
    name: String,
    timestamp_ns: u64,
    values: HashMap<String, String>,
}

fn render(value: &FieldValueRef<'_>) -> String {
    match value {
        FieldValueRef::Varint(v) => v.to_string(),
        FieldValueRef::F64(v) => v.to_string(),
        FieldValueRef::None => "<absent>".to_owned(),
        other => format!("{other:?}"),
    }
}

/// Drive `intervals` readouts through a real source into a sealed trace.
///
/// Calls `sample` directly rather than going through a live flush thread, so
/// the number of readouts is exactly `intervals` instead of whatever the flush
/// cadence happens to produce. The gate itself is covered separately.
fn run(intervals: usize, config: MetricsRsConfig, mut record: impl FnMut(usize)) -> Vec<Event> {
    let metrics_recorder = recorder();
    let mut source = MetricsRsSource::new(metrics_recorder.clone(), config);

    let dir = tempfile::tempdir().unwrap();
    let writer = DiskBuffer::single_file(dir.path().join("trace.bin")).unwrap();
    let dial9_recorder = dial9_core::recorder::recorder(writer).build();
    dial9_recorder.enable();
    let handle = dial9_recorder.handle().clone();

    for interval in 0..intervals {
        metrics::with_local_recorder(&metrics_recorder, || record(interval));
        handle.with_encoder(|encoder| source.sample(encoder));
    }
    dial9_recorder.graceful_shutdown(Duration::from_secs(5));

    decode(dir.path())
}

/// No gate, so every `sample` call counts.
fn config() -> MetricsRsConfig {
    MetricsRsConfig::builder()
        .interval(Duration::ZERO)
        .percentiles(vec![0.5])
        .build()
}

fn decode(dir: &std::path::Path) -> Vec<Event> {
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
                .map(|(def, value)| (def.name().to_owned(), render(value)))
                .collect();
            events.push(Event {
                name: event.name.to_owned(),
                timestamp_ns: event.timestamp_ns,
                values,
            });
        });
    }
    events
}

fn for_metric<'a>(events: &'a [Event], name: &str) -> Vec<&'a Event> {
    events.iter().filter(|event| event.name == name).collect()
}

fn series(events: &[Event], name: &str, field: &str) -> Vec<String> {
    for_metric(events, name)
        .iter()
        .map(|event| event.values[field].clone())
        .collect()
}

#[test]
fn counters_go_on_the_wire_as_a_running_total() {
    // The readout reports 2, 2, 2 (it swaps to zero each time). What the
    // viewer's counter kind wants is the cumulative series.
    let events = run(3, config(), |_| {
        metrics::counter!("requests_total").increment(2);
    });

    assert_eq!(
        series(&events, "metricsrs:requests_total", "value"),
        vec!["2", "4", "6"]
    );
}

#[test]
fn a_quiet_counter_is_carried_at_its_total() {
    // Metrique drops zero-delta counters from the readout entirely, so the
    // source carries the total to keep the line going.
    let events = run(4, config(), |interval| {
        if interval == 0 {
            metrics::counter!("requests_total").increment(5);
        }
    });

    assert_eq!(
        series(&events, "metricsrs:requests_total", "value"),
        vec!["5", "5", "5", "5"]
    );
}

#[test]
fn a_quiet_gauge_is_carried_at_its_last_reading() {
    let events = run(3, config(), |interval| {
        if interval == 0 {
            metrics::gauge!("pool_available").set(4.0);
        }
    });

    assert_eq!(
        series(&events, "metricsrs:pool_available", "value"),
        vec!["4", "4", "4"]
    );
}

#[test]
fn a_quiet_histogram_carries_count_but_blanks_its_percentiles() {
    let events = run(3, config(), |interval| {
        if interval == 0 {
            metrics::histogram!("query_ms").record(40.0);
        }
    });

    assert_eq!(
        series(&events, "metricsrs:query_ms", "count"),
        vec!["1", "1", "1"]
    );
    let percentiles = series(&events, "metricsrs:query_ms", "p50");
    assert_eq!(percentiles[0], "40");
    assert_eq!(&percentiles[1..], ["<absent>", "<absent>"]);
}

#[test]
fn a_histogram_that_reports_again_gets_its_percentile_back() {
    let events = run(3, config(), |interval| {
        if interval != 1 {
            metrics::histogram!("query_ms").record(40.0);
        }
    });

    let percentiles = series(&events, "metricsrs:query_ms", "p50");
    assert_eq!(percentiles, vec!["40", "<absent>", "40"]);
}

#[test]
fn the_interval_gate_skips_most_flushes() {
    // The flush thread calls far more often than the interval. All but one of
    // those has to become an immediate return.
    let mut source = MetricsRsSource::new(
        recorder(),
        MetricsRsConfig::builder()
            .interval(Duration::from_secs(1))
            .build(),
    );
    let start = std::time::Instant::now();

    assert!(source.should_sample(start), "the first call always samples");
    assert!(!source.should_sample(start + Duration::from_millis(5)));
    assert!(!source.should_sample(start + Duration::from_millis(999)));
    assert!(source.should_sample(start + Duration::from_millis(1000)));
    // The gate measures from the last sample, not from the start.
    assert!(!source.should_sample(start + Duration::from_millis(1500)));
    assert!(source.should_sample(start + Duration::from_millis(2000)));
}

#[test]
fn a_zero_interval_samples_every_flush() {
    let mut source = MetricsRsSource::new(recorder(), config());
    let start = std::time::Instant::now();

    assert!(source.should_sample(start));
    assert!(source.should_sample(start));
}

#[test]
fn every_metric_in_one_readout_shares_a_timestamp() {
    let events = run(1, config(), |_| {
        metrics::counter!("a").increment(1);
        metrics::counter!("b").increment(1);
        metrics::gauge!("c").set(1.0);
    });

    assert_eq!(events.len(), 3);
    let stamps: Vec<_> = events.iter().map(|event| event.timestamp_ns).collect();
    assert!(
        stamps.windows(2).all(|pair| pair[0] == pair[1]),
        "{stamps:?}"
    );
}

#[test]
fn new_metrics_past_the_cap_are_dropped_and_the_rest_keep_going() {
    let config = MetricsRsConfig::builder()
        .interval(Duration::ZERO)
        .max_metrics(2)
        .build();
    let events = run(2, config, |interval| {
        metrics::counter!("a").increment(1);
        metrics::counter!("b").increment(1);
        if interval == 1 {
            metrics::counter!("over_the_cap").increment(1);
        }
    });

    assert!(for_metric(&events, "metricsrs:over_the_cap").is_empty());
    // The two already tracked are unaffected.
    assert_eq!(series(&events, "metricsrs:a", "value"), vec!["1", "2"]);
    assert_eq!(series(&events, "metricsrs:b", "value"), vec!["1", "2"]);
}

#[test]
fn metric_order_is_stable_across_intervals() {
    // Emission follows first-seen order rather than HashMap iteration order,
    // so a trace does not shuffle its events between readouts.
    let events = run(3, config(), |_| {
        metrics::counter!("zebra").increment(1);
        metrics::counter!("alpha").increment(1);
        metrics::counter!("middle").increment(1);
    });

    let names: Vec<_> = events.iter().map(|event| event.name.as_str()).collect();
    let first = &names[..3];
    assert_eq!(&names[3..6], first);
    assert_eq!(&names[6..9], first);
}

#[test]
fn a_metric_appearing_late_starts_from_its_own_first_reading() {
    let events = run(3, config(), |interval| {
        metrics::counter!("early").increment(1);
        if interval == 2 {
            metrics::counter!("late").increment(9);
        }
    });

    assert_eq!(
        series(&events, "metricsrs:early", "value"),
        vec!["1", "2", "3"]
    );
    assert_eq!(series(&events, "metricsrs:late", "value"), vec!["9"]);
}

#[test]
fn the_sampling_interval_is_reported_as_segment_metadata() {
    // A consumer reading the trace should not have to infer the resolution
    // from event timestamps.
    use dial9_core::source::Source;

    let mut source = MetricsRsSource::new(
        recorder(),
        MetricsRsConfig::builder()
            .interval(Duration::from_millis(250))
            .build(),
    );

    let mut out = Vec::new();
    source.segment_metadata(&mut out);
    assert_eq!(
        out,
        vec![("metrics_rs.interval_ms".to_string(), "250".to_string())]
    );

    // Change-aware: the flush loop merges only when something was appended.
    out.clear();
    source.segment_metadata(&mut out);
    assert!(out.is_empty());
}

#[test]
fn a_counter_and_gauge_sharing_a_name_keep_the_first() {
    // metrics.rs holds counters, gauges and histograms in separate
    // registries, so one name can be two metrics. The wire schema binds to
    // whichever is seen first; the other cannot be recorded under that name.
    let events = run(2, config(), |_| {
        metrics::counter!("shared").increment(1);
        metrics::gauge!("shared").set(99.0);
    });

    let shared = for_metric(&events, "metricsrs:shared");
    assert_eq!(shared.len(), 2, "one event per interval, not two");
    // The counter sorts first in the readout, so it owns the name and keeps
    // accumulating. The gauge's 99 never appears.
    assert_eq!(series(&events, "metricsrs:shared", "value"), vec!["1", "2"]);
}
