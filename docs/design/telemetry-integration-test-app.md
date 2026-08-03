# Telemetry Integration Test Application

Status: proposed

## Summary

Add a dedicated workspace application at `examples/telemetry-test-app` that
produces one self-describing trace covering:

- CPU profiling;
- spans; and
- task dumps.

The application is both a normal runnable example and the producer for an
end-to-end conformance test. Its trace contains the measurement window,
scenario catalog, and expected relationships. Stable function names encode
relative CPU and wait-time weights. Fixture events encode expectations that
cannot be expressed by stack symbols, such as span topology and task-dump
branch roles.

The same trace bytes are evaluated through two production paths:

1. the local JavaScript parser and analysis helpers used by the viewer; and
2. the Rust decode, Parquet aggregation, and query path used for aggregate
   traces.

Each path produces the same normalized observation model. A shared validator
compares each observation independently with the contract parsed from the
trace, then checks local/aggregate parity.

This application and both conformance adapters are prerequisites for
task-dump mixed flamegraphs. The flamegraph implementation must extend this
fixture first, rather than introducing another purpose-built trace.

## Motivation

The demo application is optimized for showing the product, not for declaring
machine-checkable telemetry semantics. Small encoder-built fixtures are useful
for decoder edge cases, but they do not exercise real profiling, symbolization,
async suspension, runtime instrumentation, or trace flushing together.

A feature-specific integration app would solve the immediate task-dump test
need but would repeat this problem for the next telemetry feature. One
long-lived fixture gives new telemetry features a standard place to define a
real workload and prove local/aggregate behavior against the same trace.

## Goals

- Exercise real dial9 producers, trace encoding, symbolization, parsing, and
  analysis in one application.
- Cover CPU profiles, spans, and task dumps in the first version.
- Make expected behavior discoverable from trace contents without source-code
  knowledge or a sidecar manifest.
- Validate the same trace through both local and aggregate production paths.
- Test task and scenario isolation as well as whole-trace aggregation.
- Separate exact structural assertions from statistically sampled assertions.
- Make adding a telemetry scenario additive and mechanically testable.
- Keep the application runnable by a developer who wants to inspect its trace
  in the viewer.

## Non-goals

- Replacing focused unit tests or small malformed-trace fixtures.
- Replacing `metrics-service` as the viewer demo.
- Producing byte-for-byte identical traces across runs.
- Using benchmark timing as a correctness oracle.
- Testing every visual detail of the viewer UI.
- Hiding unavailable profiling capabilities by silently skipping assertions.

## Artifact and Invocation

Add `examples/telemetry-test-app` as a workspace member. It must write to a
local trace directory and require no AWS account, database, or network service.
The canonical invocation is release mode so symbols and timing resemble a
deployed application:

```bash
cargo run --release -p telemetry-test-app -- \
    --trace-dir target/telemetry-test-trace \
    --cycles 80 \
    --workers 4 \
    --task-dump-captures-per-second-per-worker 20 \
    --seed 1
```

The public CLI uses a builder-backed private configuration. It includes:

- trace directory;
- warm-up and measured cycle counts;
- runtime worker count;
- task-dump capture rate;
- deterministic workload seed; and
- phase quantum duration.

Defaults should produce a trace with enough samples for interactive inspection.
CI may select shorter phase durations while enforcing minimum CPU-sample and
task-dump counts before applying statistical assertions.

The output is only trace segments. The application and harness neither produce
nor consume a sidecar manifest: it could drift independently and would not be
available when a user opens an arbitrary captured fixture.

## Trace-Contained Contract

The contract has two complementary parts:

1. weighted function names for expectations represented by callchains; and
2. dedicated fixture events for lifecycle and relationships.

The contract reader is deliberately smaller than either production analysis
path. It decodes scalar fixture events and parses weighted tokens registered by
those events, but it does not derive CPU, span, or task-dump observations. This
avoids using the behavior under test as its own oracle.

### Weighted symbol names

Profiled and suspended leaf functions use this token grammar:

```text
dial9_fixture_<domain>_<bucket>_weight_<positive integer>
```

Initial domains are `cpu` and `wait`. Example symbols are:

```text
dial9_fixture_cpu_primary_weight_1
dial9_fixture_cpu_secondary_weight_3
dial9_fixture_wait_timer_weight_3
dial9_fixture_wait_notification_weight_2
```

Rust module paths and async closure suffixes may surround the token. Each
expected token is registered by a fixture symbol event before measurement. The
contract parser takes the expected set from those events, parses each token's
weight, and rejects duplicate or malformed registrations. Production
observations must then recover every registered token from a symbolized frame;
the expected set is never inferred from the sampled output.

Weights are relative within the event-declared scenario and subject, not
global percentages. For weights `1` and `3`, the expected shares are `25%` and
`75%` regardless of the configured phase quantum. Relative weights keep the
contract readable when scenarios are extended and avoid percentages that no
longer sum correctly.

Leaf functions are `#[inline(never)]`; CPU leaves use `black_box` where needed.
Release builds retain debuginfo. A fixture self-test fails if every declared
weighted symbol is not recovered from a canonical trace.

### Fixture events

Use a `telemetry.fixture` namespace for event type names and fields so
production events cannot be mistaken for fixture control data.

One `TelemetryFixtureConfigEvent` defines:

- contract version;
- run identifier and deterministic seed;
- configured worker count; and
- configured phase quantum and cycle counts.

`TelemetryFixturePhaseEvent` carries only the run identifier and lifecycle
phase: `warmup_start`, `measurement_start`, `measurement_end`, or
`shutdown_complete`. Its timestamps define the authoritative measurement
window. Tests do not infer an attach-time delay. Task-dump analysis further
clips the window to the per-worker sampling-active metadata defined by the
task-dump sampling design.

`TelemetryFixtureScenarioEvent` defines:

- feature and scenario name;
- subject, such as a task label or thread label;
- iteration number; and
- `start` or `end` phase.

These events identify exact scenario windows and counts. They also allow local
and aggregate tests to ask the same scoped questions without hard-coded
timestamps.

`TelemetryFixtureSymbolEvent` registers:

- domain, subject, and relative-weight group; and
- the exact stable symbol token expected in sampled callchains.

The event does not repeat the numeric weight; that remains part of the function
name. Registration makes an entirely missing bucket detectable while keeping
the expected ratio human-readable in ordinary stack output.

Feature-specific contract events carry relationships that callchains cannot:

- span name, parent span, count per iteration, and expected fields;
- task-dump branch role (`application`, `timeout`, `cancellation`, or `peer`)
  and expected representative category; and
- explicit noise subjects that must be excluded from a task-scoped result.

These records are typed rather than an arbitrary assertion expression
language. A future telemetry feature adds its own typed contract event and
normalized observation section. Unknown contract versions, feature records,
properties, or enum values are errors; the harness must not silently ignore
contract data.

New fields may be added to the self-describing trace schemas. JavaScript
contract readers must treat fields absent from an older fixture as
`undefined`, consistent with trace-format compatibility rules.

### Contract integrity

Scenario definitions should drive both workload execution and contract-event
emission from one in-process value. Do not maintain a second list in the test
harness.

Before testing telemetry output, the harness validates:

- exactly one measurement start and end for a run;
- balanced, non-overlapping start/end events for each scenario iteration;
- known contract version and feature records;
- positive, complete relative-weight groups;
- unique run, scenario, and subject identifiers; and
- all measurement scenarios falling inside the measurement window.

This makes a malformed fixture a clear producer failure rather than a parser
regression.

## Workload Design

The application uses synchronized phases, not sleep-based startup assumptions.
It records a warm-up interval, waits for profiler and runtime instrumentation
to become active, runs measured cycles, records the measurement end, flushes
all trace sources, and finally records shutdown completion.

The common phase quantum is intentionally configurable. Increasing it improves
statistical confidence without changing expected relative weights.

### CPU profiling

Two instrumented async tasks use different CPU mixes:

```text
task_a: dial9_fixture_cpu_primary_weight_1
        dial9_fixture_cpu_secondary_weight_3

task_b: dial9_fixture_cpu_primary_weight_3
        dial9_fixture_cpu_secondary_weight_1
```

Each CPU leaf busy-loops against a measured monotonic deadline. Cycle order is
rotated from the deterministic seed to reduce systematic boundary bias.

A named uninstrumented OS thread runs
`dial9_fixture_cpu_process_noise_weight_1`. It must appear in the process CPU
profile and must not appear in either task-scoped profile. This simultaneously
tests whole-process accounting and task isolation.

The CPU contract asserts relative sample weight after clipping samples to
fixture scenario windows. It does not assert exact sample counts or exact
timestamps.

### Spans

Every measured cycle emits a stable span topology:

```text
fixture_cycle
  fixture_cpu_phase
  fixture_async_phase
    fixture_nested_child
  fixture_parallel_child
  fixture_parallel_child
```

The span scenario covers:

- exact parent/child relationships;
- repeated sibling names;
- overlapping sibling lifetimes;
- a span held across async suspension;
- integer, string, boolean, and duration fields;
- task labels shared with CPU and task-dump scenarios; and
- one span outside the measurement window that must be excluded.

Expected topology, count per iteration, and field values come from typed span
contract events. Span durations are checked for ordering and containment, not
exact wall-clock values.

### Task dumps

The task-dump scenario includes:

- standalone timer wait;
- standalone notification wait;
- asynchronous wait for blocking-lock work;
- `timeout(primary_operation, deadline)`;
- `select!` over application work and cancellation;
- `select!` over two peer application branches; and
- two task labels with different weighted wait mixes.

The timeout and cancellation records identify their control branches. The peer
case declares that no branch is preferred and therefore expects one
deterministic `[awaiting any of N]` category. Standalone timer and notification
scenarios prove those future types are not unconditionally treated as control
flow.

Enough concurrent tasks run to exercise every worker that receives eligible
polls. Tests derive the participating worker set from trace events and sampler
metadata; they never assign task dumps to an assumed worker.

The weighted wait symbols declare expected relative idle time. Capture counts
remain random. Inverse-probability weighted idle time is the value compared
with the declared mix.

### Cross-feature correlation

CPU and wait phases run inside the declared span tree and use the same stable
task labels. This permits assertions that:

- a selected task has the expected CPU and async-idle components;
- enclosing spans are found consistently by local and aggregate analysis;
- data from the second task and noise thread do not bleed into the selection;
  and
- whole-run totals equal the sum of disjoint measured scenario windows within
  statistical tolerance.

## Normalized Observation Model

Both adapters emit versioned JSON with these logical sections:

```text
run
  measurement window and observed worker set
cpu
  scope, subject, scenario, weighted symbol, sample count, estimated time
spans
  scenario, span identity, parent identity, fields, start/end offsets
task_dumps
  task, scenario, capture group, representative category,
  inclusion probability, estimated idle time
```

Timestamps are represented as offsets from `measurement_start`; process
wall-clock and monotonic epochs are never compared directly. Symbols are
normalized to the stable fixture token, not full platform-dependent demangled
names. Collections have deterministic ordering.

### Local adapter

The local adapter invokes `trace_parser.js` and the production analysis helpers
used by the viewer. It must not add a fixture-only event decoder for CPU
samples, spans, or task dumps. Fixture control events may be extracted by the
small contract reader.

### Aggregate adapter

The aggregate adapter sends every generated segment through the production
Rust decode and Parquet ingest path, waits for complete source coverage, and
queries the same public aggregation interfaces used by the server. Calling
only an in-memory decoder is insufficient.

The canonical fixture remains below aggregate sampling caps where practical.
If a production aggregation stage samples data, the normalized result carries
and applies its production weight rather than bypassing that stage.

### Comparison order

The harness performs three comparisons:

1. local observation against the embedded contract;
2. aggregate observation against the embedded contract; and
3. local observation against aggregate observation.

The first two identify implementations that agree with each other but are both
wrong. The third gives a focused parity failure when only one path diverges.

## Assertion Policy

Structural properties are exact:

- scenario presence and measurement boundaries;
- worker and task identity where the contract declares them;
- span counts, topology, field values, and containment;
- task-dump capture grouping and representative categories;
- weighted symbol discovery;
- local/aggregate scope and filter behavior; and
- absence of excluded subjects.

Sampled properties use explicit statistical bounds:

- CPU sample shares are compared with expected relative weights using the
  observed sample count;
- task-dump idle shares use inverse-probability estimates and their observed
  inclusion probabilities; and
- minimum sample thresholds fail with an instruction to increase cycles or
  capture rate, rather than widening tolerances until a sparse trace passes.

The validator owns the confidence level and calculation. Individual tests do
not embed hand-tuned percentage ranges. A second task-dump capture-rate run
must change capture count and variance without materially changing the
estimated wait shares.

Exact trace digests are inappropriate because sample timestamps and counts are
intentionally nondeterministic.

## Extending the Fixture

Each telemetry feature owns one scenario module in the application. Adding a
scenario requires:

1. workload code and stable names;
2. trace-contained contract records;
3. local normalized-observation support;
4. aggregate normalized-observation support; and
5. shared semantic assertions.

The integration harness fails on an emitted feature record that has no adapter.
This prevents a new workload from looking covered while neither parser checks
it.

Semantic changes to existing contract records increment the contract version.
Adding a scenario under existing semantics is additive. Old canonical traces
remain readable, but CI validates the current fixture with the current
contract.

## Fixture and CI Workflow

Add `scripts/regenerate_telemetry_test_trace.sh` to build and run the
application, concatenate its segments in order, and run both conformance
adapters. It produces a dedicated canonical fixture; it does not overwrite the
viewer demo trace.

CI has two layers:

1. All platforms run local and aggregate conformance against the committed
   canonical fixture.
2. A profiling-capable Linux job regenerates a trace from the application and
   runs the same conformance suite against the fresh output.

The capture job must fail if CPU profiling or task-dump capture is unavailable.
A reduced-capability trace is not promoted as canonical and does not silently
skip the affected feature.

The committed fixture is regenerated whenever producer behavior, fixture
scenarios, trace schemas, or relevant symbolization changes. Statistical
validation means a regenerated trace need not match the committed bytes.

## Delivery Order

1. Add the application skeleton, lifecycle events, scenario registry, and
   weighted-symbol contract.
2. Implement CPU, span, and current task-dump workloads.
3. Add the contract reader, normalized observation schema, and shared
   validator.
4. Add the local JavaScript adapter.
5. Add the Rust decode/Parquet/query adapter.
6. Add canonical-fixture regeneration and both CI layers.
7. Extend the task-dump scenario for sampled capture probabilities and
   representative-stack selection.
8. Only then implement and validate task-dump mixed flamegraphs.
