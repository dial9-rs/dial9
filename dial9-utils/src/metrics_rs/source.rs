//! A dial9 [`Source`] that samples a metrics.rs registry.
//!
//! The flush thread calls [`Source::flush`] on its own cadence, an interval
//! gate decides when that turns into a readout. Anything that has to survive
//! between readouts lives here.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use dial9_core::clock::clock_monotonic_ns;
use dial9_core::encoder::ThreadLocalEncoder;
use dial9_core::rate_limited;
use dial9_core::source::{FlushContext, Source};
use dial9_trace_format::types::FieldValue;
use metrique_metricsrs::MetricRecorder;

use super::capture::{MetricSample, Percentile, SampleValue, capture};
use super::encode::{Schemas, write};

const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_MAX_METRICS: usize = 512;
const DEFAULT_PERCENTILES: [f64; 3] = [0.5, 0.9, 0.99];

/// The `metrics::Recorder` dial9 samples.
///
/// Install it with `metrics::set_global_recorder`, alone or inside a
/// `metrics_util::layers::FanoutBuilder`, and pass a clone to
/// [`with_metrics_rs`](RecorderMetricsRsExt::with_metrics_rs). Clones share one
/// registry.
#[derive(Clone)]
pub struct MetricsRsRecorder(MetricRecorder<dyn metrics::Recorder>);

impl std::fmt::Debug for MetricsRsRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsRsRecorder").finish_non_exhaustive()
    }
}

/// Build the recorder dial9 will sample.
///
/// ```no_run
/// let recorder = dial9_utils::metrics_rs::recorder();
/// metrics::set_global_recorder(recorder.clone()).unwrap();
/// ```
pub fn recorder() -> MetricsRsRecorder {
    MetricsRsRecorder(MetricRecorder::new())
}

impl metrics::Recorder for MetricsRsRecorder {
    fn describe_counter(
        &self,
        key: metrics::KeyName,
        unit: Option<metrics::Unit>,
        description: metrics::SharedString,
    ) {
        self.0.describe_counter(key, unit, description);
    }

    fn describe_gauge(
        &self,
        key: metrics::KeyName,
        unit: Option<metrics::Unit>,
        description: metrics::SharedString,
    ) {
        self.0.describe_gauge(key, unit, description);
    }

    fn describe_histogram(
        &self,
        key: metrics::KeyName,
        unit: Option<metrics::Unit>,
        description: metrics::SharedString,
    ) {
        self.0.describe_histogram(key, unit, description);
    }

    fn register_counter(
        &self,
        key: &metrics::Key,
        metadata: &metrics::Metadata<'_>,
    ) -> metrics::Counter {
        self.0.register_counter(key, metadata)
    }

    fn register_gauge(
        &self,
        key: &metrics::Key,
        metadata: &metrics::Metadata<'_>,
    ) -> metrics::Gauge {
        self.0.register_gauge(key, metadata)
    }

    fn register_histogram(
        &self,
        key: &metrics::Key,
        metadata: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        self.0.register_histogram(key, metadata)
    }
}

/// Configuration for [`MetricsRsSource`].
#[derive(Debug, Clone, bon::Builder)]
pub struct MetricsRsConfig {
    /// Minimum time between readouts. Defaults to one second.
    ///
    /// The flush thread batches its own events well above this, so a much
    /// shorter interval buys resolution the segments will not keep.
    #[builder(default = DEFAULT_INTERVAL)]
    interval: Duration,

    /// Most metrics to track. Defaults to 512.
    ///
    /// Each one holds a wire schema, so this bounds both the source's state
    /// and dial9's trace-encoder schema cache against high label cardinality.
    #[builder(default = DEFAULT_MAX_METRICS)]
    max_metrics: usize,

    /// Quantiles reported per histogram, each in `0.0..=1.0`. Defaults to
    /// p50, p90 and p99.
    #[builder(default = DEFAULT_PERCENTILES.to_vec())]
    percentiles: Vec<f64>,
}

impl Default for MetricsRsConfig {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl MetricsRsConfig {
    /// Minimum time between readouts.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Most metrics tracked.
    pub fn max_metrics(&self) -> usize {
        self.max_metrics
    }

    /// Quantiles reported per histogram.
    pub fn percentiles(&self) -> &[f64] {
        &self.percentiles
    }
}

/// One metric's current state, and the readout it was last seen in.
#[derive(Debug)]
struct Carried {
    sample: MetricSample,
    last_seen: u64,
}

/// Flush-thread source that samples a metrics.rs registry into the trace.
///
/// Register it with [`with_metrics_rs`](RecorderMetricsRsExt::with_metrics_rs)
/// rather than constructing it directly.
pub struct MetricsRsSource {
    recorder: MetricRecorder<dyn metrics::Recorder>,
    interval: Duration,
    max_metrics: usize,
    percentiles: Vec<Percentile>,
    last_sample: Option<Instant>,

    /// Every metric seen so far, at its current value. Counters hold a running
    /// total, not the interval delta the readout reports.
    carried: HashMap<String, Carried>,
    /// Emission order, so events do not follow the map's iteration order.
    order: Vec<String>,
    /// Which readout we are on, so `absorb` can tell what reported this time
    /// without collecting names.
    generation: u64,
    at_capacity: bool,
    metadata_emitted: bool,

    schemas: Schemas,
    samples: Vec<MetricSample>,
    values: Vec<FieldValue>,
}

impl std::fmt::Debug for MetricsRsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsRsSource")
            .field("interval", &self.interval)
            .field("tracked", &self.order.len())
            .finish_non_exhaustive()
    }
}

impl MetricsRsSource {
    /// Sample `recorder` at `config`'s interval.
    pub fn new(recorder: MetricsRsRecorder, config: MetricsRsConfig) -> Self {
        Self {
            recorder: recorder.0,
            interval: config.interval,
            max_metrics: config.max_metrics,
            percentiles: config
                .percentiles
                .iter()
                .copied()
                .map(Percentile::new)
                .collect(),
            last_sample: None,
            carried: HashMap::new(),
            order: Vec::new(),
            generation: 0,
            at_capacity: false,
            metadata_emitted: false,
            schemas: Schemas::default(),
            samples: Vec::new(),
            values: Vec::new(),
        }
    }

    /// Fold one readout into the carried state, emptying `fresh`.
    fn absorb(&mut self, fresh: &mut Vec<MetricSample>) {
        self.generation += 1;
        for sample in fresh.drain(..) {
            match self.carried.get_mut(&sample.name) {
                Some(carried) => {
                    carried.last_seen = self.generation;
                    merge(&mut carried.sample, sample);
                }
                None => {
                    if self.order.len() >= self.max_metrics {
                        if !self.at_capacity {
                            self.at_capacity = true;
                            tracing::error!(
                                metric = %sample.name,
                                max_metrics = self.max_metrics,
                                "metrics.rs source is at its metric cap; this and any \
                                 further new metrics are not recorded"
                            );
                        }
                        continue;
                    }
                    self.order.push(sample.name.clone());
                    self.carried.insert(
                        sample.name.clone(),
                        Carried {
                            sample,
                            last_seen: self.generation,
                        },
                    );
                }
            }
        }

        // A metric that reported nothing keeps its counters and gauges. Its
        // percentiles go absent: a quiet interval has no latency to report.
        for carried in self.carried.values_mut() {
            if carried.last_seen == self.generation {
                continue;
            }
            for field in &mut carried.sample.fields {
                if let SampleValue::OptionalGauge(value) = &mut field.value {
                    *value = None;
                }
            }
        }
    }
}

/// Fold a fresh reading into the carried one, in place.
///
/// A metric's field list is fixed by its type, so a mismatch means two metrics
/// share a name. metrics.rs allows that, since counters, gauges and histograms
/// live in separate registries. The first one keeps it; the wire schema is
/// already bound to its types.
fn merge(carried: &mut MetricSample, fresh: MetricSample) {
    if carried.fields.len() != fresh.fields.len() {
        rate_limited!(Duration::from_secs(60), {
            tracing::warn!(
                metric = %carried.name,
                "two metrics.rs metrics share a name with different shapes; \
                 keeping the first"
            );
        });
        return;
    }

    for (carried, fresh) in carried.fields.iter_mut().zip(fresh.fields) {
        match (&mut carried.value, fresh.value) {
            // The readout swaps counters to zero, so each reading is that
            // interval's delta and the running total is ours to keep.
            (SampleValue::CounterDelta(total), SampleValue::CounterDelta(delta)) => {
                *total = total.saturating_add(delta);
            }
            (SampleValue::Gauge(current), SampleValue::Gauge(fresh)) => *current = fresh,
            (SampleValue::OptionalGauge(current), SampleValue::OptionalGauge(fresh)) => {
                *current = fresh;
            }
            _ => {
                rate_limited!(Duration::from_secs(60), {
                    tracing::warn!(
                        field = %carried.name,
                        "two metrics.rs metrics share a name with different kinds; \
                         keeping the first"
                    );
                });
            }
        }
    }
}

impl MetricsRsSource {
    /// Whether `now` is far enough past the last readout to take another.
    /// The flush thread calls far more often than this fires.
    pub(crate) fn should_sample(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last_sample
            && now.duration_since(last) < self.interval
        {
            return false;
        }
        self.last_sample = Some(now);
        true
    }

    /// Take a readout and write every tracked metric, ignoring the interval
    /// gate. Split out so tests can drive it without a flush thread.
    pub(crate) fn sample(&mut self, encoder: &mut ThreadLocalEncoder<'_>) {
        // Taken out and put back so the buffer keeps its capacity while
        // `absorb` holds `&mut self`.
        let mut samples = std::mem::take(&mut self.samples);
        let readout = self.recorder.readout();
        capture(&readout, &self.percentiles, &mut samples);
        self.absorb(&mut samples);
        self.samples = samples;

        // One timestamp for the whole readout, so every metric in it lines up
        // on the timeline.
        let timestamp_ns = clock_monotonic_ns();
        for name in &self.order {
            if let Some(entry) = self.carried.get(name) {
                write(
                    encoder,
                    &mut self.schemas,
                    timestamp_ns,
                    &entry.sample,
                    &mut self.values,
                );
            }
        }
    }
}

impl Source for MetricsRsSource {
    fn flush(&mut self, ctx: &FlushContext<'_>) {
        if !self.should_sample(Instant::now()) {
            return;
        }
        ctx.with_encoder(|encoder| self.sample(encoder));
    }

    fn name(&self) -> &'static str {
        "metrics_rs"
    }

    fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
        if self.metadata_emitted {
            return;
        }
        self.metadata_emitted = true;
        // The interval is the resolution of every metricsrs series. Without it
        // a consumer has to infer the cadence from event timestamps, which is
        // wrong for a metric that started late or one carried through a
        // dropped flush.
        out.push((
            "metrics_rs.interval_ms".to_string(),
            self.interval.as_millis().to_string(),
        ));
    }
}

/// `.with_metrics_rs(..)` sugar for plugging [`MetricsRsSource`] into a
/// recorder builder in one call.
pub trait RecorderMetricsRsExt: Sized {
    /// Sample `recorder` into the trace at `config`'s interval.
    fn with_metrics_rs(self, recorder: MetricsRsRecorder, config: MetricsRsConfig) -> Self;
}

impl<T: dial9_core::recorder::RecorderSourceExt> RecorderMetricsRsExt for T {
    fn with_metrics_rs(self, recorder: MetricsRsRecorder, config: MetricsRsConfig) -> Self {
        self.source(MetricsRsSource::new(recorder, config))
    }
}
