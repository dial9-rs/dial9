# Self-Describing Telemetry Integration Test Application

Status: proposed

## Summary

The test application is an executable description of its expected trace.
Stable function and span names describe trace structure and relative weights;
small expectation events record relationships that names alone cannot express.
The test harness contains no fixture-specific table of expected symbols,
percentages, or span edges.

A test first parses the expected model from the trace itself. It then parses
the same bytes through:

1. the production JavaScript path used for local traces; and
2. the production Rust decode and Parquet path used for aggregate traces.

Both observed results are checked against the trace's declared model. This
makes the captured trace a portable, self-describing integration fixture.

The first application remains deliberately small: one nested workload in which
CPU samples, spans, and task dumps overlap. It must land before task-dump mixed
flamegraphs, which can consume the already-declared CPU/wait relationships.

## Core Idea

The important artifact is not a particular demo workload. It is the
convention that lets a trace describe what should be found inside it.

For example:

```text
dial9_fixture_cpu_inner_weight_3
```

declares that the symbol belongs to the CPU domain, is named `inner`, and has
relative weight `3`. The test does not separately hard-code those facts.

Trace data supplies the rest of the structure:

- symbol names declare domains, identities, and relative weights;
- function callchains declare parent/child stack structure;
- span names and expectation events declare span nesting and which span should
  be active for a symbol; and
- marker events declare the measurement window.

The harness knows only this convention. Adding a named operation to the
program and registering it in the trace extends the expected model without
adding a second manifest that can drift.

Expectation registration is necessary because an item missing from the
captured data cannot announce its own absence. The registration event names
the expected item and relationships, while the function name remains the
source of its weight.

## Minimal Mixed Workload

Run one dial9-instrumented async task. After warm-up, repeat this nested
execution:

```text
function dial9_fixture_mixed_cycle
`-- span dial9_fixture_span_cycle
    |-- CPU       dial9_fixture_cpu_outer_weight_1
    |-- TASK_DUMP dial9_fixture_wait_outer_weight_1
    `-- function dial9_fixture_mixed_inner
        `-- span dial9_fixture_span_inner
            |-- CPU       dial9_fixture_cpu_inner_weight_3
            `-- TASK_DUMP dial9_fixture_wait_inner_weight_2
```

CPU functions busy-loop for their declared number of quanta. Wait functions
use `tokio::time::sleep` for their declared number of quanta. The cycle span is
open across the entire sequence; the inner span is open across both its CPU
work and its await.

This is one mixed trace, not three adjacent feature demos. It declares:

- CPU weights of `1:3`;
- async-wait weights of `1:2`;
- an overall CPU-to-wait relationship of `4:3`;
- CPU samples and task dumps directly inside the cycle span;
- CPU samples and task dumps directly inside the nested inner span; and
- a span that remains active across async suspension.

The initial integration test needs only presence, hierarchy, and coarse
relative-weight assertions. Task-dump mixed-flamegraph tests can later consume
the same names to check the `4:3` whole-cycle and `3:2` inner-subtree mixes.

Use `#[inline(never)]` and `black_box` where needed so release builds retain
the fixture function hierarchy.

## Trace Convention

Weighted functions use:

```text
dial9_fixture_<domain>_<name>_weight_<positive integer>
```

The initial domains are `cpu` and `wait`. Unweighted fixture parent functions
use `dial9_fixture_mixed_<name>`. Spans use
`dial9_fixture_span_<name>`.

The application emits two fixture-specific event types.

`TelemetryFixtureExpectationEvent` records:

- `feature`: `cpu`, `task_dump`, or `span`;
- `name`: the stable function or span token;
- optional `parent`: the expected fixture parent function or parent span; and
- optional `active_span`: the innermost span expected to contain observations
  of this symbol. Ancestor containment follows the declared span-parent edges.

`TelemetryFixtureMarkerEvent` records:

- `phase`: `measurement_start` or `measurement_end`.

Expectation events are emitted before measurement. Marker timestamps define
the interval used for all three telemetry sources. Numeric weights are not
repeated in events; they are parsed from function names.

A tiny expectation reader converts these events and names into:

```text
expected symbols and weights
expected fixture stack edges
expected span parent edges
expected symbol-to-span associations
measurement start/end
```

This reader does not inspect CPU samples, spans, or task dumps to derive
expectations. Those are observations produced independently by the two
production parsing paths.

There is no sidecar manifest, contract version, scenario registry, or general
assertion language. These event types are private to the test application.

## Application

Add `examples/telemetry-test-app` as a workspace binary. Its only required
option is a trace directory; cycle count is optional:

```bash
cargo run --release -p telemetry-test-app -- \
    --trace-dir target/telemetry-test-trace \
    --cycles 40
```

Worker count, task-dump capture rate, and phase durations can be constants
until a test needs to vary them. The application requires no AWS account,
database, or network service and writes no assertion artifact besides trace
segments.

The first version does not need multiple tasks, noise threads, timeout or
cancellation branches, worker-coverage cases, or capture-rate sweeps.

## Integration Test

One profiling-capable Linux test:

1. runs the application once in release mode;
2. collects the generated trace segments;
3. reads the declared model from fixture events and names;
4. parses the same bytes with `trace_parser.js`;
5. sends the bytes through the Rust aggregate decoder and Parquet
   writer/reader; and
6. compares each observed result with the declared model.

Do not stand up the HTTP server or simulated S3. Those paths have separate
coverage.

Each production path returns only the test-local facts needed here:

```text
CPU fixture stacks and sample counts
task-dump fixture stacks
span parent edges and fields
symbol-to-span associations
```

The JavaScript check can print this shape as JSON for the Rust test to compare.
It is not a versioned library interface.

### Initial assertions

- both measurement markers are present and ordered;
- every registered symbol, stack edge, span edge, and span association appears
  in both parsing paths;
- both CPU and task-dump observations occur under the cycle and inner spans;
- the long CPU function has more samples than the short CPU function after a
  minimum total sample count;
- the cycle span retains its cycle field;
- at least one task dump contains each wait symbol; and
- local and aggregate results agree on the registered structure.

Do not initially assert exact counts, timestamps, ratios, task migration, or
worker distribution. Focused tests own decoder edge cases and statistical
estimators.

The test fails if CPU profiling or task dumps are unavailable. Other platforms
do not need a checked-in trace fallback initially.

## Growing the Application

Add a workload operation only when a telemetry feature needs an end-to-end
case. Give it a descriptive fixture name, register its expected relationships
in the trace, and teach the generic comparison only about a new kind of
relationship if the existing convention cannot express it.

Keep the workload linear until repetition justifies a scenario abstraction.
Task-dump branch-selection cases remain focused tests unless an end-to-end
regression shows that one belongs here.

## Delivery Order

1. Add the mixed application, naming convention, and two fixture event types.
2. Add the expectation reader and local JavaScript check.
3. Add the aggregate decode/Parquet check to the same test.
4. Add the profiling-capable Linux CI invocation.
5. Use the declared CPU/wait structure to validate task-dump mixed
   flamegraphs.
