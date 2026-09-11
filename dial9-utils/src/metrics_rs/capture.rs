//! Turning one metrics.rs readout into per-metric trace events.
//!
//! A readout has no descriptors, since its field names are metric names known
//! only at runtime. Nothing here needs them: names arrive on the `value`
//! callback and labels on `metric`.
//!
//! One [`MetricSample`] per metric, because a schema name binds to one field
//! list for a whole segment and a metric's own fields never change.

use std::borrow::Cow;
use std::time::{Duration, SystemTime};

use dial9_core::rate_limited;
use metrique_writer::{
    Entry, EntryConfig, EntryWriter, MetricFlags, Observation, Unit, ValidationError, Value,
    ValueWriter,
};

/// Unit annotation key. Same one the `TraceEvent` derive emits.
pub(crate) const UNIT_ANNOTATION_KEY: &str = "unit";

/// Separator between labels inside a field name.
/// Not a comma since the viewer uses it as a field-chart URL separator.
const LABEL_SEPARATOR: char = ';';

/// Field name for a counter or gauge. Histograms name their own.
pub(crate) const VALUE_FIELD: &str = "value";

/// One metric's data, which becomes one trace event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MetricSample {
    /// Metric plus labels: `requests_total`, `errors{kind=timeout}`. Becomes
    /// the schema name under the `metricsrs:` prefix.
    pub(crate) name: String,
    pub(crate) fields: Vec<Field>,
}

/// One field of that event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Field {
    pub(crate) name: Cow<'static, str>,
    /// Set when the application called `describe_counter!` or friends.
    pub(crate) unit: Option<&'static str>,
    pub(crate) value: SampleValue,
}

/// A field's value. Also fixes its wire type and `kind` annotation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SampleValue {
    /// This interval's delta. `readout()` swaps counters to zero and drains
    /// histograms, so the running total is the source's to keep.
    CounterDelta(u64),
    /// The reading at readout time.
    Gauge(f64),
    /// Gauge-shaped but skippable. Only percentiles: an interval with no
    /// samples has no latency to report.
    OptionalGauge(Option<f64>),
}

/// A requested quantile and the field name it produces.
///
/// Built once from config so `percentile_field_name` does not run per readout.
#[derive(Debug, Clone)]
pub(crate) struct Percentile {
    quantile: f64,
    field: String,
}

impl Percentile {
    /// `0.5` becomes `p50`, `0.999` becomes `p99.9`.
    pub(crate) fn new(quantile: f64) -> Self {
        Self {
            field: percentile_field_name(quantile),
            quantile,
        }
    }
}

/// Capture `entry` into `out`, replacing its previous contents.
pub(crate) fn capture(entry: &impl Entry, percentiles: &[Percentile], out: &mut Vec<MetricSample>) {
    out.clear();
    entry.write(&mut SampleWriter { out, percentiles });
}

struct SampleWriter<'a> {
    out: &'a mut Vec<MetricSample>,
    percentiles: &'a [Percentile],
}

impl<'e> EntryWriter<'e> for SampleWriter<'_> {
    fn timestamp(&mut self, _: SystemTime) {
        // Events are stamped with the monotonic clock at sample time, so
        // metrique's wall clock is not carried.
    }

    fn config(&mut self, _: &'e dyn EntryConfig) {
        // `AllowSplitEntries` is for EMF's per-record cap. dial9 has none.
    }

    fn value(&mut self, name: impl Into<Cow<'e, str>>, value: &(impl Value + ?Sized)) {
        let metric = name.into();
        value.write(ObservationCapture {
            metric: &metric,
            out: self.out,
            percentiles: self.percentiles,
        });
    }
}

struct ObservationCapture<'a> {
    metric: &'a str,
    out: &'a mut Vec<MetricSample>,
    percentiles: &'a [Percentile],
}

impl ValueWriter for ObservationCapture<'_> {
    fn string(self, _: &str) {
        // metrics.rs has no string-valued metric type.
        rate_limited!(Duration::from_secs(60), {
            tracing::debug!(
                metric = %self.metric,
                "metrics.rs readout produced a string value; skipped"
            );
        });
    }

    fn error(self, error: ValidationError) {
        rate_limited!(Duration::from_secs(60), {
            tracing::warn!(
                metric = %self.metric,
                %error,
                "metrics.rs readout produced an invalid value; field skipped"
            );
        });
    }

    fn metric<'a>(
        self,
        distribution: impl IntoIterator<Item = Observation>,
        unit: Unit,
        dimensions: impl IntoIterator<Item = (&'a str, &'a str)>,
        _flags: MetricFlags<'_>,
    ) {
        let name = metric_name(self.metric, dimensions);
        let unit = unit_annotation_value(unit);
        let scalar = |value| {
            vec![Field {
                name: Cow::Borrowed(VALUE_FIELD),
                unit,
                value,
            }]
        };

        // Collected, not streamed: a histogram's shape is only clear once
        // every observation is in.
        //
        // The match reads the metric's type off that shape. Metrique's readout
        // is specific about it: counters emit one `Unsigned`, gauges one
        // `Floating`, histograms one `Repeated` per bucket.
        let observations: Vec<Observation> = distribution.into_iter().collect();
        let fields = match observations.as_slice() {
            // A histogram with no samples this interval. The source carries
            // its previous event, total included.
            [] => return,
            [Observation::Unsigned(v)] => scalar(SampleValue::CounterDelta(*v)),
            [Observation::Floating(v)] => scalar(SampleValue::Gauge(*v)),
            observations if observations.iter().all(is_bucket) => {
                match histogram_fields(&name, unit, observations, self.percentiles) {
                    Some(fields) => fields,
                    None => return,
                }
            }
            _ => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        metric = %name,
                        "metrics.rs readout produced an unrecognised observation mix; \
                         metric skipped"
                    );
                });
                return;
            }
        };
        self.out.push(MetricSample { name, fields });
    }
}

fn is_bucket(observation: &Observation) -> bool {
    matches!(observation, Observation::Repeated { .. })
}

/// Build the metric's event name.
///
/// Commas are replaced, not rejected. The viewer joins a chart's event and
/// field name with one, so a name carrying a comma cannot be linked to. Label
/// values are the likely source, and the series is worth more than the name.
fn metric_name<'a>(
    metric: &str,
    dimensions: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    let mut labels: Vec<(&str, &str)> = dimensions.into_iter().collect();
    let mut name = sanitized(metric);
    if !labels.is_empty() {
        // Sorted so the same label set always produces the same event name,
        // and therefore the same wire schema, across intervals.
        labels.sort_unstable();
        name.push('{');
        for (i, (key, value)) in labels.iter().enumerate() {
            if i > 0 {
                name.push(LABEL_SEPARATOR);
            }
            name.push_str(&sanitized(key));
            name.push('=');
            name.push_str(&sanitized(value));
        }
        name.push('}');
    }
    name
}

/// Swap commas for the label separator this naming scheme already uses.
fn sanitized(part: &str) -> String {
    if !part.contains(',') {
        return part.to_owned();
    }
    rate_limited!(Duration::from_secs(60), {
        tracing::debug!(
            part,
            "metrics.rs name or label contains a comma, which the viewer cannot put \
             in a chart link; replacing it with ';'"
        );
    });
    part.replace(',', &LABEL_SEPARATOR.to_string())
}

/// Expand a drained histogram into `count`, `sum`, and one field per requested
/// percentile. `None` when the histogram took no samples this interval.
fn histogram_fields(
    metric: &str,
    unit: Option<&'static str>,
    observations: &[Observation],
    percentiles: &[Percentile],
) -> Option<Vec<Field>> {
    // Metrique reports each bucket as its midpoint repeated `occurrences`
    // times, so the midpoint divides back out. Bucket values are `u32`
    // upstream, so every midpoint and the sum are whole numbers.
    let mut buckets: Vec<(u64, u64)> = observations
        .iter()
        .filter_map(|observation| match observation {
            Observation::Repeated {
                total,
                occurrences: occurrences @ 1..,
            } => Some(((total / *occurrences as f64).round() as u64, *occurrences)),
            _ => None,
        })
        .collect();
    // Percentiles read a cumulative count, so order by value, not by whatever
    // the drain yielded.
    buckets.sort_unstable();

    let count: u64 = buckets.iter().map(|(_, occurrences)| occurrences).sum();
    if count == 0 {
        return None;
    }
    // Saturating because a flush-thread source must not panic, and both
    // factors are `u32`-derived, so their product can reach `u64::MAX`.
    let sum: u64 = buckets.iter().fold(0u64, |sum, (midpoint, occurrences)| {
        sum.saturating_add(midpoint.saturating_mul(*occurrences))
    });

    if sum == 0 {
        // Genuine zeros and truncated fractions are indistinguishable once
        // drained, so this reports what was seen (rather than naming a cause).
        // The likely one: `HistogramFn::record` casts through `u32`, so an
        // application recording seconds (`0.012`) loses every sample.
        rate_limited!(Duration::from_secs(60), {
            tracing::warn!(
                metric = %metric,
                count,
                "every metrics.rs histogram sample is zero; if the values are \
                 fractional they are being truncated by the u32 cast, so record \
                 in smaller units (ms or us)"
            );
        });
    }

    let mut fields = Vec::with_capacity(2 + percentiles.len());
    fields.push(Field {
        // The count of a millisecond histogram is not itself milliseconds.
        name: Cow::Borrowed("count"),
        unit: Some("count"),
        value: SampleValue::CounterDelta(count),
    });
    fields.push(Field {
        name: Cow::Borrowed("sum"),
        unit,
        value: SampleValue::CounterDelta(sum),
    });
    for percentile in percentiles {
        fields.push(Field {
            name: Cow::Owned(percentile.field.clone()),
            unit,
            value: SampleValue::OptionalGauge(Some(quantile(&buckets, percentile.quantile) as f64)),
        });
    }
    Some(fields)
}

/// The bucket midpoint at `percentile`, by cumulative count.
///
/// `buckets` must be sorted by midpoint and hold at least one observation.
fn quantile(buckets: &[(u64, u64)], percentile: f64) -> u64 {
    let total: u64 = buckets.iter().map(|(_, occurrences)| occurrences).sum();
    let target = ((total as f64) * percentile).ceil().max(1.0) as u64;
    let mut seen = 0;
    for &(midpoint, occurrences) in buckets {
        seen += occurrences;
        if seen >= target {
            return midpoint;
        }
    }
    buckets.last().map_or(0, |(midpoint, _)| *midpoint)
}

/// `0.5` becomes `p50`, `0.999` becomes `p99.9`. Two decimal places of
/// percent, trailing zeros trimmed, so the name does not depend on how the
/// quantile happens to be represented in binary.
fn percentile_field_name(percentile: f64) -> String {
    let mut percent = format!("{:.2}", percentile * 100.0);
    while percent.ends_with('0') {
        percent.pop();
    }
    if percent.ends_with('.') {
        percent.pop();
    }
    format!("p{percent}")
}

/// Map a metrique unit to the vocabulary the viewer formats
/// (`us`/`ms`/`s`/`bytes`), plus `count` (rendered plain). Units outside it
/// keep their CloudWatch name and render unformatted. `Unit::None` carries no
/// annotation at all, since metrics.rs only knows a unit when the application
/// called `describe_*`.
fn unit_annotation_value(unit: Unit) -> Option<&'static str> {
    use metrique_writer::core::unit::{NegativeScale, PositiveScale};
    match unit {
        Unit::None => None,
        Unit::Second(NegativeScale::Micro) => Some("us"),
        Unit::Second(NegativeScale::Milli) => Some("ms"),
        Unit::Second(NegativeScale::One) => Some("s"),
        Unit::Byte(PositiveScale::One) => Some("bytes"),
        Unit::Count => Some("count"),
        other => Some(other.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrique_metricsrs::{MetricRecorder, ParametricRecorder};

    fn default_percentiles() -> Vec<Percentile> {
        [0.5, 0.9, 0.99].into_iter().map(Percentile::new).collect()
    }

    /// Record through a local metrics.rs recorder and capture one readout.
    /// This is the real input shape: the readout's `Entry` impl is what the
    /// source walks in production.
    fn capture_from(record: impl FnOnce()) -> Vec<MetricSample> {
        capture_with(&default_percentiles(), record)
    }

    fn capture_with(percentiles: &[Percentile], record: impl FnOnce()) -> Vec<MetricSample> {
        let recorder: MetricRecorder<dyn metrics::Recorder> = MetricRecorder::new();
        recorder.with_local_recorder(record);
        let mut out = Vec::new();
        capture(&recorder.readout(), percentiles, &mut out);
        out
    }

    fn names(samples: &[MetricSample]) -> Vec<&str> {
        samples.iter().map(|s| s.name.as_str()).collect()
    }

    fn metric<'a>(samples: &'a [MetricSample], name: &str) -> &'a MetricSample {
        samples
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no metric {name} in {:?}", names(samples)))
    }

    fn field<'a>(samples: &'a [MetricSample], name: &str, field: &str) -> &'a Field {
        let sample = metric(samples, name);
        sample
            .fields
            .iter()
            .find(|f| f.name == field)
            .unwrap_or_else(|| panic!("no field {field} on {name}"))
    }

    fn field_names(samples: &[MetricSample], name: &str) -> Vec<String> {
        metric(samples, name)
            .fields
            .iter()
            .map(|f| f.name.to_string())
            .collect()
    }

    #[test]
    fn counter_records_its_interval_delta() {
        let samples = capture_from(|| {
            metrics::counter!("requests_total").increment(5);
            metrics::counter!("requests_total").increment(3);
        });

        assert_eq!(names(&samples), vec!["requests_total"]);
        assert_eq!(
            field(&samples, "requests_total", VALUE_FIELD).value,
            SampleValue::CounterDelta(8)
        );
    }

    #[test]
    fn labels_sort_into_the_metric_name() {
        // Declared out of alphabetical order: the same label set has to
        // produce the same name every interval, or the wire schema churns.
        let samples = capture_from(|| {
            metrics::counter!("errors", "kind" => "timeout", "code" => "504").increment(1);
        });

        assert_eq!(names(&samples), vec!["errors{code=504;kind=timeout}"]);
    }

    #[test]
    fn each_label_combination_is_its_own_metric() {
        let samples = capture_from(|| {
            metrics::counter!("errors", "kind" => "timeout").increment(4);
            metrics::counter!("errors", "kind" => "refused").increment(1);
        });

        assert_eq!(
            field(&samples, "errors{kind=timeout}", VALUE_FIELD).value,
            SampleValue::CounterDelta(4)
        );
        assert_eq!(
            field(&samples, "errors{kind=refused}", VALUE_FIELD).value,
            SampleValue::CounterDelta(1)
        );
    }

    #[test]
    fn gauge_records_its_reading_not_a_delta() {
        let samples = capture_from(|| {
            metrics::gauge!("pool_available").set(7.0);
            metrics::gauge!("pool_available").decrement(2.0);
        });

        assert_eq!(
            field(&samples, "pool_available", VALUE_FIELD).value,
            SampleValue::Gauge(5.0)
        );
    }

    #[test]
    fn zero_delta_counters_are_absent_from_the_readout() {
        // Metrique drops them unless `emit_zero_counters`, which is what makes
        // the live metric set change mid-trace.
        let samples = capture_from(|| {
            metrics::counter!("never_incremented").increment(0);
        });

        assert!(samples.is_empty(), "{samples:#?}");
    }

    #[test]
    fn histogram_is_one_metric_with_count_sum_and_percentiles() {
        let samples = capture_from(|| {
            for value in [10.0, 20.0, 30.0, 40.0] {
                metrics::histogram!("query_ms").record(value);
            }
        });

        assert_eq!(names(&samples), vec!["query_ms"]);
        assert_eq!(
            field_names(&samples, "query_ms"),
            vec!["count", "sum", "p50", "p90", "p99"]
        );
        assert_eq!(
            field(&samples, "query_ms", "count").value,
            SampleValue::CounterDelta(4)
        );
    }

    #[test]
    fn histogram_count_and_sum_accumulate_but_percentiles_do_not() {
        // The variant fixes both the wire type and the `kind` annotation, so
        // this is what makes count chart as a counter and p50 as a gauge.
        let samples = capture_from(|| metrics::histogram!("query_ms").record(10.0));

        assert!(matches!(
            field(&samples, "query_ms", "count").value,
            SampleValue::CounterDelta(_)
        ));
        assert!(matches!(
            field(&samples, "query_ms", "sum").value,
            SampleValue::CounterDelta(_)
        ));
        assert!(matches!(
            field(&samples, "query_ms", "p50").value,
            SampleValue::OptionalGauge(Some(_))
        ));
    }

    #[test]
    fn histogram_percentiles_track_the_true_distribution() {
        // Lognormal-ish latency spread, the shape these are actually read for.
        let mut values = Vec::new();
        let mut state: u64 = 12345;
        for _ in 0..100_000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let uniform = ((state >> 33) as f64) / (u32::MAX as f64 / 2.0);
            values.push((1.0 + (uniform * 8.0).exp().min(5000.0)) as u64);
        }

        let samples = capture_from(|| {
            for value in &values {
                metrics::histogram!("lat_ms").record(*value as f64);
            }
        });

        values.sort_unstable();
        for (quantile, name) in [(0.5, "p50"), (0.9, "p90"), (0.99, "p99")] {
            let exact = values[((values.len() - 1) as f64 * quantile).round() as usize] as f64;
            let SampleValue::OptionalGauge(Some(got)) = field(&samples, "lat_ms", name).value
            else {
                panic!("{name} should be a present percentile");
            };
            let error = (got - exact).abs() / exact;
            // Buckets are 16 per binary order of magnitude and reported at
            // their midpoint, which halves the 6.25% bucket width.
            assert!(
                error < 0.032,
                "{name}: got {got}, exact {exact}, error {:.2}%",
                error * 100.0
            );
        }

        assert_eq!(
            field(&samples, "lat_ms", "count").value,
            SampleValue::CounterDelta(values.len() as u64)
        );
    }

    #[test]
    fn histogram_with_no_samples_records_nothing() {
        // Registering without recording drains to an empty bucket list.
        let samples = capture_from(|| {
            let _registered = metrics::histogram!("query_ms");
        });

        assert!(samples.is_empty(), "{samples:#?}");
    }

    #[test]
    fn sub_unit_histogram_values_truncate_to_zero() {
        // Upstream `HistogramFn::record` casts through `u32`. An application
        // recording seconds loses every sample; the capture still reports the
        // count so the diagnostic has something to point at.
        let samples = capture_from(|| {
            for _ in 0..1000 {
                metrics::histogram!("lat_secs").record(0.012);
            }
        });

        assert_eq!(
            field(&samples, "lat_secs", "count").value,
            SampleValue::CounterDelta(1000)
        );
        assert_eq!(
            field(&samples, "lat_secs", "sum").value,
            SampleValue::CounterDelta(0)
        );
        assert_eq!(
            field(&samples, "lat_secs", "p99").value,
            SampleValue::OptionalGauge(Some(0.0))
        );
    }

    #[test]
    fn commas_are_replaced_rather_than_dropping_the_metric() {
        // The viewer joins a chart's event and field name with a comma, so a
        // name carrying one cannot be linked to.
        let samples = capture_from(|| {
            metrics::counter!("a,b").increment(1);
            metrics::counter!("ok", "path" => "/a,/b").increment(1);
        });

        let mut got = names(&samples);
        got.sort_unstable();
        assert_eq!(got, vec!["a;b", "ok{path=/a;/b}"]);
        assert!(got.iter().all(|name| !name.contains(',')));
    }

    #[test]
    fn described_units_reach_the_fields() {
        let samples = capture_from(|| {
            metrics::describe_histogram!("query_ms", metrics::Unit::Milliseconds, "");
            metrics::describe_counter!("bytes_read", metrics::Unit::Bytes, "");
            metrics::histogram!("query_ms").record(12.0);
            metrics::counter!("bytes_read").increment(4096);
            metrics::counter!("undescribed").increment(1);
        });

        assert_eq!(
            field(&samples, "bytes_read", VALUE_FIELD).unit,
            Some("bytes")
        );
        assert_eq!(field(&samples, "query_ms", "p50").unit, Some("ms"));
        assert_eq!(field(&samples, "query_ms", "sum").unit, Some("ms"));
        // The count of a millisecond histogram is not itself milliseconds.
        assert_eq!(field(&samples, "query_ms", "count").unit, Some("count"));
        assert_eq!(field(&samples, "undescribed", VALUE_FIELD).unit, None);
    }

    #[test]
    fn percentile_field_names_trim_trailing_zeros() {
        assert_eq!(percentile_field_name(0.5), "p50");
        assert_eq!(percentile_field_name(0.9), "p90");
        assert_eq!(percentile_field_name(0.99), "p99");
        assert_eq!(percentile_field_name(0.999), "p99.9");
        assert_eq!(percentile_field_name(1.0), "p100");
    }

    #[test]
    fn requested_percentiles_drive_the_fields() {
        let percentiles: Vec<_> = [0.75, 0.999].into_iter().map(Percentile::new).collect();
        let samples = capture_with(&percentiles, || {
            metrics::histogram!("query_ms").record(12.0);
        });

        assert_eq!(
            field_names(&samples, "query_ms"),
            vec!["count", "sum", "p75", "p99.9"]
        );
    }

    #[test]
    fn quantile_reads_the_cumulative_count() {
        let buckets = [(10, 1), (20, 1), (30, 1), (40, 1)];
        assert_eq!(quantile(&buckets, 0.0), 10);
        assert_eq!(quantile(&buckets, 0.25), 10);
        assert_eq!(quantile(&buckets, 0.5), 20);
        assert_eq!(quantile(&buckets, 1.0), 40);
    }

    #[test]
    fn capture_replaces_previous_contents() {
        let recorder: MetricRecorder<dyn metrics::Recorder> = MetricRecorder::new();
        let mut out = vec![MetricSample {
            name: "stale".to_owned(),
            fields: Vec::new(),
        }];

        recorder.with_local_recorder(|| metrics::counter!("fresh").increment(1));
        capture(&recorder.readout(), &default_percentiles(), &mut out);

        assert_eq!(names(&out), vec!["fresh"]);
    }
}
