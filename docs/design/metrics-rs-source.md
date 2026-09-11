# metrics.rs Source Design

Issue: [dial9-rs/dial9#676](https://github.com/dial9-rs/dial9/issues/676)

Status: **implemented.**

## Motivation

For applications that already track their own state in metrics.rs. Sampling it into the trace puts it on the same timeline as runtime telemetry, so a queue spike can be read against `inflight_requests` rather than against a dashboard at a different resolution. Ideally call sites shouldn't change and the wiring would just be done in the recorder at startup.

## Non-goals

- Per-increment events: `record_event`, the metrique sink and the tracing layer cover those, and better, a metrics.rs increment would only carry something like a name and a number.

## Crate placement

`dial9-utils` behind a `metrics-rs` feature. Not re-exported from the `dial9` facade, which does not depend on `dial9-utils`. This feature shouldn't need a Tokio runtime.

## User-facing API

```rust
// The ext trait is what puts `with_metrics_rs` on the builder.
use dial9_utils::metrics_rs::{self, MetricsRsConfig, RecorderMetricsRsExt};

let metrics_recorder = metrics_rs::recorder();
// Or inside a `metrics_util::layers::FanoutBuilder` next to an existing backend.
metrics::set_global_recorder(metrics_recorder.clone())?;

let dial9_recorder = dial9_core::recorder::recorder(writer)
    .with_metrics_rs(metrics_recorder, MetricsRsConfig::default())
    .build();
```

`MetricsRsConfig` carries `interval` (1s), `max_metrics` (512) and `percentiles` (p50/p90/p99).

Installing into metrics.rs is up to the caller, deliberately not something `with_metrics_rs` does for them. Whether to compose with `FanoutBuilder` is theirs to decide. The recorder is `Arc`-backed, so their copy and the source's share one registry.

## Architecture

**Uses metrique's collection.** `metrique_metricsrs::MetricRecorder` is the `metrics::Recorder`: sharded registry, an `Arc<AtomicU64>` per counter and gauge, an atomic bucketed histogram, a units map from `describe_*`. dial9 writes no recorder, registry or histogram.

**Uses dial9's sampling.** `MetricsRsSource` implements `dial9_core::Source` and runs on the flush thread, which ticks well below the sampling interval. An interval gate (default 1s) decides when to sample: `readout()`, collect one `MetricSample` per metric, fold into the carried state, then one `write_event` per metric.

### Why not `Dial9Stream` or `MetricReporter`

`Dial9Stream` only takes entries whose shape is fixed at compile time, because it uses that shape as a cache key and matches values to fields by position. A metrics.rs readout doesn't have that type of shape: its field names are the application's own metric names, so `descriptors()` is `Unavailable` and the entry is dropped before a value is read. Feeding `MetricReporter` into it would record nothing.

Writing our own reader works where `Dial9Stream` does not, because the entry does carry its metric names but somewhere else: `Entry::write` passes each one to `value(name, ..)` and `capture.rs` reads them there instead of from the descriptor.

`MetricReporter` is itself a timer that pushes readouts into a metrique sink, dial9's flush thread is already a timer, so using it would mean two, plus a tokio runtime and a second shutdown to order against the recorder's. `Dial9Stream` takes the push route for a reason that does not apply here: metrique entries arrive when a request closes, so there is nothing to sample.

## Event shape

**One schema per metric plus label combination**, named `metricsrs:<metric>{labels}`.

```
metricsrs:requests_total          value
metricsrs:errors{kind=timeout}    value
metricsrs:query_ms                count, sum, p50, p90, p99
```

Labels are sorted for stability and joined with `;`. Any comma in a metric name or label is replaced with the same `;`, because the viewer identifies a chart by its event and field name joined with a comma, so a name carrying one cannot be linked to. The prefix keeps application names out of dial9's event namespace, where a metric called `ClockSyncEvent` would collide. Each field carries a `kind` annotation so the viewer charts it, and a `unit` annotation when the application called `describe_*`.

Per-metric because a schema name binds to one field list for the life of a segment. Registering a second shape under one name fails, which is what a single shared `MetricsRs` event would hit:

```
Err("schema already registered with different definition: MetricsRs")
```

Its field list would be fixed by the first sample of each segment, and metrics register lazily, so that sample holds the fewest of them. Every later sample that saw a new metric appear would be rejected. A metric's own field set never grows, so per-metric schemas satisfy the constraint by construction. It also groups better: the chart picker keys on (event name, field name), so a histogram is one pickable group rather than five loose entries.

The **event name is a compat surface**, persisted in chart links and saved track preferences. Here it derives from the application's metric name.

Costs: one event per metric per interval, and one schema per metric against dial9's trace encoder, which caches 1024 schemas and drops the lot once it passes that, and which `max_metrics` stays under. One timestamp is taken per readout and shared by its events, so metrics stay aligned.

| metrics.rs | `readout()` returns | On the wire |
| --- | --- | --- |
| counter | interval delta (swapped to 0) | running cumulative total, `kind=counter` |
| gauge | current value | as-is, `kind=gauge` |
| histogram | drained bucket tally | `count`, `sum` (`kind=counter`), percentiles (`kind=gauge`) |

Counters go on the wire cumulative rather than as the delta metrique hands back. The viewer takes the difference between consecutive points itself for a `counter` field, and detects resets, cumulative stays correct across a dropped interval, and it matches what Prometheus and OpenTelemetry mean by a counter. Costs one `HashMap` of totals.

### Histograms

A drained tally has one entry per non-empty bucket, so its length follows how far the distribution spreads, not how many samples it took. Each arrives as `Observation::Repeated { total, occurrences }`, where `total` is the bucket's midpoint times its count, so the midpoint is `total / occurrences`.

Percentiles are computed here rather than stored raw. A test in `capture.rs` compares them against exact quantiles over 100k lognormal latency samples: p50 -1.8%, p90 -1.4%, p99 -0.3%, p99.9 +1.6%, sum -0.05%, count exact.

The cost is that percentiles cannot be re-aggregated: sixty per-second p99s do not average into a p99 for the minute. That cuts against the aggregate-at-query-time principle in `CONTEXT.md`, which earns its keep for the CPU samples table because that spans many hosts and hours. One trace is one process over one segment, and raw buckets stay an additive change if that stops being true.

## Carrying quiet metrics

A counter that did not move contributes nothing to a readout, and a histogram that took no samples drains empty. Emitting nothing would draw a line that blinks in and out, so once a metric has been seen its event is emitted every interval: counters at their running total, gauges at their last reading, histograms at an unchanged `count` and `sum`.

Percentiles are the exception. They are optional fields and go absent in an interval with no samples, because carrying the last one forward would claim a latency nothing measured.

State is per-metric, so a metric appearing late touches no other's schema.

## Segment metadata

The source reports one key, `metrics_rs.interval_ms`, once per segment. Inferring the resolution from event timestamps instead would be wrong for a metric that registered late or one carried through a dropped flush.

## Limits and diagnostics

Unbounded label cardinality means unbounded metrics, so `max_metrics` caps how many are tracked, well below the 1024 schemas dial9's trace encoder caches before dropping the lot.

| Condition | Behaviour |
| --- | --- |
| Tracked metrics over `max_metrics` | new metrics ignored, one `error!` naming the first |
| Histogram drains to all zeros | rate-limited `warn!` |
| Two metrics share a name | first keeps it, rate-limited `warn!` |
| Encoder rejects an event | dropped, rate-limited `error!` |

The all-zeros case is the only signal available for a bug that already exists in metrique: its `HistogramFn::record` casts the recorded `f64` through `u32`, so an application recording `Duration::as_secs_f64()` loses every sample to bucket zero. Nothing here introduces it, but this source is what puts it on a chart. A truncated fraction and a genuine zero look identical once drained, so the warning reports what it saw rather than naming a cause.

Two metrics can share a name because metrics.rs keeps counters, gauges and histograms in separate registries.

## Future work

- Fix metrique's histogram truncation (the `u32` cast above). We could swap its fixed integer buckets for a float sketch like DDSketch, make the storage pluggable so only dial9 opts in, or have metrique warn on sub-unit records. The sketch is the biggest change and would move existing metrique users' percentiles, so it is not obviously the one to pick.
- Put raw buckets on the wire. A `DynamicList` field carrying the tally, leaving percentiles to the viewer. This is additive to the format, so we can take that approach if per-interval percentiles turn out too lossy.
