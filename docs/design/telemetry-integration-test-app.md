# Telemetry Integration Test Application

Status: proposed

## Summary

Add one small workspace application at `examples/telemetry-test-app`. It
produces a real trace containing CPU profiles, spans, and task dumps. One
integration test sends the same trace through:

1. the production JavaScript parser used for local traces; and
2. the production Rust decode and Parquet path used for aggregate traces.

The first version is a tracer bullet, not a fixture framework. It has one
workload, two small fixture event types, and a handful of assertions. New
scenarios should be added only when a telemetry feature needs them.

This application and the basic local/aggregate test must land before task-dump
mixed flamegraphs. The flamegraph work can then extend the application with
the specific cases it needs.

## Goals

- Exercise real capture, encoding, symbolization, and both parsing paths.
- Cover CPU profiles, spans, and task dumps in one trace.
- Keep expectations in the trace rather than in a sidecar manifest.
- Make the trace useful when opened manually in the viewer.
- Establish one obvious place to add future end-to-end telemetry cases.

## Non-goals

- A general scenario registry or assertion language.
- A stable, versioned fixture protocol.
- Exhaustive coverage of each telemetry feature.
- A checked-in canonical trace.
- Testing the HTTP server, S3, or viewer rendering.
- Replacing focused synthetic and unit tests.

## Minimal Workload

Run one dial9-instrumented async task on a small Tokio runtime. After a short
warm-up, repeat this measured cycle:

```text
dial9_fixture_span_cycle
  dial9_fixture_span_cpu
    dial9_fixture_cpu_short_weight_1
    dial9_fixture_cpu_long_weight_3
  dial9_fixture_span_wait
    dial9_fixture_wait_short_weight_1
    dial9_fixture_wait_long_weight_2
```

The CPU functions busy-loop for one and three quanta. The wait functions use
`tokio::time::sleep` for one and two quanta. A cycle field is recorded on the
root span.

This is enough to prove:

- periodic CPU samples preserve recognizable weighted symbols;
- parent and child spans with a field survive parsing;
- task dumps preserve recognizable async wait symbols; and
- all three sources can describe the same task and time interval.

Use `#[inline(never)]` and `black_box` where needed to retain the function
names in release builds. The app does not initially include multiple tasks,
noise threads, overlapping spans, timeout/cancellation branches, worker
coverage cases, or multiple capture rates.

## Self-Describing Trace

Weights remain in stable function-name tokens:

```text
dial9_fixture_<domain>_<name>_weight_<positive integer>
```

The test parses the weight from the symbol. For example, the two CPU functions
declare a `1:3` expected relationship without a hard-coded percentage in the
test.

The app emits only two fixture-specific event types:

`TelemetryFixtureExpectationEvent`

- `feature`: `cpu`, `span`, or `task_dump`;
- `name`: expected symbol or span name; and
- optional `parent`: expected parent span name.

`TelemetryFixtureMarkerEvent`

- `phase`: `measurement_start` or `measurement_end`.

Expectation events are emitted before measurement. They make a completely
missing symbol or span detectable; expected items are not inferred from
whatever samples happened to be captured. Marker timestamps define the
measurement window. There is no separate config event, run identifier,
contract version, scenario event, or sidecar manifest.

These events are test-app implementation details, not public dial9 interfaces.
If this representation becomes awkward after adding real scenarios, change it
then.

## Invocation

The app requires only a trace directory and an optional cycle count:

```bash
cargo run --release -p telemetry-test-app -- \
    --trace-dir target/telemetry-test-trace \
    --cycles 40
```

Worker count, task-dump capture rate, and phase durations may be constants in
the first implementation. They should become options only when a test needs
to vary them.

The application requires no AWS account, database, or network service. It
writes trace segments and no other assertion artifact.

## Integration Test

One script or Rust integration test:

1. runs the application once in release mode;
2. collects the generated trace segments;
3. parses those bytes with `trace_parser.js`;
4. sends the same bytes through the Rust aggregate decoder and Parquet
   writer/reader; and
5. checks both results against the expectation and marker events.

Do not stand up the HTTP server or simulated S3 for this test. Those paths have
their own integration coverage.

Each parser returns only the small result needed here:

```text
measurement start/end
CPU sample count by fixture symbol
span (name, parent, fields)
task-dump symbol set
```

The JavaScript check can print this test-local shape as JSON for the Rust test
to compare. Do not turn it into a versioned library interface until another
test needs one.

### Initial assertions

Keep the first assertions deliberately coarse:

- both measurement markers are present and ordered;
- every expectation event has a matching observation in both paths;
- the long CPU function has more samples than the short CPU function after a
  minimum total sample count;
- the CPU and wait spans are children of the cycle span, and the cycle span
  retains its cycle field;
- at least one task dump contains each wait symbol; and
- local and aggregate results agree on expected symbol and span presence.

Do not assert exact sample counts, exact timestamps, exact ratios, task
migration, or worker distribution. Focused tests remain responsible for
decoder edge cases and statistical estimators.

The test fails if profiling or task dumps are unavailable. Run it in one
profiling-capable Linux CI job; other platforms do not need a committed trace
fallback initially.

## Growing the App

When a feature needs an end-to-end case, add the smallest workload phase,
expectation event, and assertion that proves it. Keep the app linear until
repetition makes a scenario abstraction useful.

In particular, task-dump mixed flamegraphs should add their weighted wait
checks and any required multi-branch await case to this app as part of that
implementation. The initial prerequisite only establishes that a real task
dump reaches both parsing paths.

## Delivery Order

1. Add the application, weighted functions, spans, and two fixture event
   types.
2. Add one test that runs it and checks the local JavaScript path.
3. Add the aggregate decode/Parquet check to the same test.
4. Add the profiling-capable Linux CI invocation.
5. Build task-dump flamegraph scenarios on this working slice.
