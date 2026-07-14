# Schema-Driven Viewer Specs

Status: working design draft.

Goal: let traces describe computed values and recommended viewer panels so adding common visualizations does not require event-specific frontend code. Simple validation cases include process CPU, socket queue utilization, and context-switch rates; the design should also leave room for current hardcoded viewer concepts such as queue depth, event markers, span interval bars, worker lanes, scheduling overlays, heatmaps, and eventually stack-based views like flamegraphs.

The viewer still owns rendering. Rust/schema metadata should describe data semantics, computed values, and recommended views, not Canvas implementation details.

## Core Model

Three layers:

1. Field semantics: units, metric kind, display labels, and event-local computed fields.
2. Computed series: ordered points or intervals derived from one or more event streams.
3. View specs: recommended generic panels such as `time_series`, `interval_bars`, `markers`, `heatmap`, and `flamegraph`.

Stable terms:

| Term | Meaning |
| --- | --- |
| `computed field` | A value computed for one event, without looking at other events. |
| `series` | A sequence of points or intervals to draw/analyze. |
| `computed series` | A series produced from event streams and expressions. |
| `multi-series` | One panel with more than one series, e.g. Queue Depth. |
| `source` | Named event stream used by a spec. |
| `point` | One output item produced by a computed series. |
| `view` | One recommended viewer panel. |
| `display` | Generic panel type and rendering hints. |
| `mark` | Shape used for one series inside a display: `line`, `step_line`, `step_area`, `bars`, `points`. |

Recommended bundle shape:

```js
{
  computed_fields: [ /* event-local computed values */ ],
  views: [ /* recommended viewer panels */ ]
}
```

`computed_fields` are intentionally outside `series`: they should be available anywhere an event is shown, including the generic/custom events UI.

## Metric Vocabulary

Use metric words as semantics, not as direct rendering instructions.

| `metric.kind` | Meaning | Example |
| --- | --- | --- |
| `gauge` | Value observed at this timestamp. May go up or down. | queue depth, socket backlog utilization, current RSS |
| `counter` | Cumulative value that should only increase. | `user_cpu_ns`, bytes sent, page faults |
| `up_down_counter` | Cumulative value that may increase or decrease. | active tasks derived from spawn/terminate deltas |

For now, `counter` implies monotonic cumulative values. Add `temporality` later only if we need delta counters.

```js
metric: { kind: "counter" }
```

Potential future extension:

```js
metric: {
  kind: "counter",
  temporality: "delta" // "cumulative" is the assumed default for counters
}
```

## Units

`unit` controls value formatting and an optional display suffix. Known units get richer formatting; custom units are shown literally. Omit it when the label already communicates everything needed.

Initial useful units:

| Unit | Meaning |
| --- | --- |
| `ns` | Nanoseconds, formatted as human duration. |
| `us` | Microseconds, formatted as human duration. |
| `ms` | Milliseconds, formatted as human duration. |
| `s` | Seconds, formatted as human duration. |
| `bytes` | Bytes, formatted as human byte size. |
| `cores` | Logical CPU cores. |
| `%` | Percent; values may exceed `100` unless the expression explicitly clamps them. |

These tokens also describe the input scale: `us` means the expression produces microseconds. Add a separate multiplier only if the wire representation must use a different scale from its semantic unit; computed expressions should normally produce their declared unit directly.

## Expression Scopes

Specs should avoid ambiguous field access.

Source aliases:

```js
sources: [
  { id: "usage", event: "ProcessResourceUsageEvent" }
]
```

The simple `event: "Name"` source form is an exact event-name match. Structured matchers such as event prefix or schema family can cover event families like tracing span callsite schemas without changing the rest of the spec shape.

Qualified field access:

```js
expr: "rate(usage.user_cpu_ns + usage.system_cpu_ns, usage.timestamp)"
```

Single-source shorthand may be accepted by a future string parser:

```js
expr: "rate(user_cpu_ns + system_cpu_ns, timestamp)"
```

Global metadata access:

```js
expr: "metadata['process.available_parallelism']"
```

Use bracket syntax because metadata keys may contain dots, e.g. `process.available_parallelism`.

Computed series output access in tooltips:

```js
expr: "point.value"
expr: "point.wall_delta_ns"
expr: "point.current.usage.user_cpu_ns"
expr: "point.previous.usage.user_cpu_ns"
```

Rules:

- `usage.foo`: field from source alias `usage`.
- `usage.timestamp`: normalized source timestamp in monotonic nanoseconds, even though the wire timestamp is encoded in the event header rather than as a schema field.
- `metadata['key']`: trace/segment metadata.
- `point.foo`: field generated by the series builder.
- `point.current.<source>.<field>`: source event used for this output point.
- `point.previous.<source>.<field>`: previous source event used for this output point.
- Unqualified fields are allowed only when exactly one source exists.

View expressions may also reference a resolved series collection. The initial collection form is `{ series: "id", range: "visible" }`, where `visible` is supplied by the viewer. Collection expressions are valid only after series have been built, such as in view summaries, guides, and tooltips.

## JSON AST

Use JSON AST internally first. String expressions can compile to this AST later via `jsep`, Rhai, or another parser.

Long examples below sometimes use string shorthand for readability. Those strings are design notation until a string expression syntax is selected.

Arithmetic:

```js
{
  op: "add",
  args: [
    { field: "usage.user_cpu_ns" },
    { field: "usage.system_cpu_ns" }
  ]
}
```

Rate over event order:

```js
{
  op: "rate",
  value: {
    op: "add",
    args: [
      { field: "usage.user_cpu_ns" },
      { field: "usage.system_cpu_ns" }
    ]
  },
  time: { field: "usage.timestamp" }
}
```

Metadata lookup:

```js
{ metadata: "process.available_parallelism" }
```

Tooltip expression:

```js
{
  op: "mul",
  args: [
    {
      op: "div",
      args: [
        { field: "point.value" },
        { metadata: "process.available_parallelism" }
      ]
    },
    { const: 100 }
  ]
}
```

String shorthand equivalent:

```js
"point.value / metadata['process.available_parallelism'] * 100"
```

Scalar `max` compares its arguments:

```js
{ op: "max", args: [{ field: "point.value" }, { const: 1 }] }
```

Collection aggregation is explicit so it cannot be confused with scalar math:

```js
{
  op: "aggregate",
  fn: "max",
  over: { series: "cores", range: "visible" },
  value: { field: "point.value" }
}
```

An aggregate returns a scalar and can be nested in arithmetic. A possible string shorthand for the example above is `aggregate('max', series['cores'], range['visible'], point.value)`. `duration_weighted_mean` weights interval points by their overlap with the requested range. Empty collections return `null`. Initial aggregation supports unpartitioned series.

Minimum expression operations:

| Operation | Notes |
| --- | --- |
| field access | `usage.user_cpu_ns`, `point.value` |
| metadata access | `metadata['process.available_parallelism']` |
| arithmetic | `+`, `-`, `*`, `/` |
| scalar `min`, `max`, `sum`, `avg` | Operations over scalar arguments. |
| `clamp(value, min, max)` | Bounds a scalar value. |
| `rate(value, time)` | `(current(value) - previous(value)) / (current(time) - previous(time))` |
| `aggregate(fn, over, value)` | Reduces values from a resolved series; initial reducers are `max` and `duration_weighted_mean`. |

Additional expression operations:

| Operation | Use |
| --- | --- |
| `delta(value)` | Current minus previous. |
| `lag(value, n = 1)` | Previous value lookup. |
| aggregate `min`, `sum`, `avg` | Additional collection reducers. |
| point selection | Read `first`, `last`, or an indexed point from a collection. |
| `count_active(start, end, key)` | Active set/time-series derivation. |
| `pair_intervals(startEvent, endEvent, key)` | Build interval bars from event pairs. |

## Missing Data And Diagnostics

If a required source, field, metadata key, or expression input is missing, the viewer should skip the affected series or view and report a diagnostic. It should not invent plausible defaults.

If an expression fails for one point because an input is missing, non-numeric, division by zero, or non-finite, skip that point unless the spec explicitly defines a semantic fallback.

## Computed Fields

Computed fields are event-local. They do not mutate raw wire fields.

Computed field specs attach to event types. Source aliases are local to the spec that declares them.

Expressions resolve raw and computed fields through the same field namespace, but runtime storage may keep computed values separate from raw fields.

Recommended runtime shape:

```js
event.fields = {
  user_cpu_ns: "100",
  system_cpu_ns: "50"
};

event.computed = {
  cpu_time_ns: 150
};

event.computed_units = {
  cpu_time_ns: "ns"
};

event.computed_labels = {
  cpu_time_ns: "CPU time"
};
```

Example spec:

```js
{
  id: "process_resource_usage.cpu_time_ns",
  source: { id: "usage", event: "ProcessResourceUsageEvent" },
  name: "cpu_time_ns",
  expr: "usage.user_cpu_ns + usage.system_cpu_ns",
  unit: "ns",
  metric: { kind: "counter" },
  label: "CPU time",
  on_collision: "error"
}
```

Computed fields can feed computed series:

```js
expr: "rate(usage.cpu_time_ns, usage.timestamp)"
```

Collision rule: a computed field should not silently overwrite a raw field.

## Series And View Specs

Bundle shape:

```js
{
  computed_fields: [
    {
      id: "process_resource_usage.cpu_time_ns",
      source: { id: "usage", event: "ProcessResourceUsageEvent" },
      name: "cpu_time_ns",
      expr: "usage.user_cpu_ns + usage.system_cpu_ns",
      unit: "ns",
      metric: { kind: "counter" }
    }
  ],
  views: [
    {
      id: "process.cpu",
      title: "CPU Usage",
      sources: [
        { id: "usage", event: "ProcessResourceUsageEvent" }
      ],
      display: {
        kind: "time_series"
      },
      series: [
        {
          id: "cores",
          expr: "rate(usage.cpu_time_ns, usage.timestamp)",
          unit: "cores",
          mark: "step_area"
        }
      ]
    }
  ]
}
```

Potential attributes:

| Attribute | Scope | Meaning |
| --- | --- | --- |
| `computed_fields` | bundle | Event-local computed values available to all event renderers. |
| `views` | bundle | Recommended viewer panels. |
| `id` | computed field/view/series | Stable identifier. |
| `title` | view/series | Human label. |
| `label` | computed field/tooltip/guide/summary/threshold | Human label. |
| `sources` | view | Event streams used by this view. |
| `source` | computed field | Single aliased event stream used by an event-local computed field. |
| `on_collision` | computed field | `error`, `skip`, or `overwrite` when a computed field name conflicts with a raw field. |
| `expr` | series/tooltip/guide/summary/threshold/computed field | Expression to evaluate. |
| `unit` | series/computed field/tooltip/guide/summary | Output unit. |
| `metric.kind` | series output/computed field/field metadata | `gauge`, `counter`, `up_down_counter`. |
| `transform` | view | Derives display items before rendering, e.g. `pair_intervals`. |
| `partition_by` | series | Split output into one series per unique key tuple. |
| `summary` | view | Labeled expressions shown as panel-level summary values. |
| `mark` | series | `line`, `step_line`, `step_area`, `bars`, `points`. |
| `color` | series | Optional fixed color or palette key. |
| `thresholds` | series | Boundaries that classify series values by level. |
| `tooltip` | series/view | Tooltip rows. |
| `display.kind` | display | `time_series`, `interval_bars`, `markers`, `heatmap`, `flamegraph`. |
| `display.y_min` | display | Optional Y-axis minimum. |
| `display.y_max` | display | Optional initial Y-domain maximum hint; observed values, guides, and thresholds may expand it. |
| `display.guides` | display | Neutral reference lines/bands; `show_value: false` displays only the label. |
| `display.color_by` | display | Field/expression used for color encoding. |
| `downsample` | series | `last_per_pixel`, `max_per_pixel`, `avg_per_pixel`, `min_max_per_pixel`. |
| `window.missing_previous` | series | `skip`, `null`, `zero` for previous-sample expressions such as `rate`. |
| `window.on_decrease` | series | `skip`, `warn`, `show` when previous-sample expressions detect an unexpected decrease. |
| `sort` | source/series | Event ordering; default is timestamp. |

`summary` is view-level because it defines scalar outputs that are independent of display geometry. Guides remain under `display` because they participate in its coordinate system and bounds. Both use the same resolved-series expression scope.

Threshold expressions are evaluated once in view scope and inherit the series unit. `time_series` renders each resolved threshold as a horizontal line, includes it in the Y domain, and applies its level to step-series coloring. Resolved guides and thresholds appear as `label (value)` in the panel legend; a guide may omit either the unit or the label.

```js
thresholds: [{
  expr: "metadata['process.available_parallelism'] * 0.8",
  level: "warning",
  label: "80% capacity"
}]
```

## Display Vocabulary

Display kinds:

| `display.kind` | Current UI example | Notes |
| --- | --- | --- |
| `time_series` | CPU Usage, Queue Depth | X axis is time, Y axis is numeric value. |
| `xy_plot` | none yet | X axis is an expression, not the trace timeline. |
| `interval_bars` | Worker lanes, Spans | Items have `start` and `end`. |
| `markers` | Events panel, wake/sched markers | Instantaneous timestamp markers. |
| `heatmap` | Poll-duration color ramp in worker lanes | Usually an encoding layered onto intervals. |
| `flamegraph` | Flamegraph sidebar | Stack aggregation, not time-axis aligned. |

`time_series` is intentionally time-axis specific so it can align with the current viewer timeline.

## Example: CPU Usage

```js
{
  computed_fields: [
    {
      id: "process_resource_usage.cpu_time_ns",
      source: { id: "usage", event: "ProcessResourceUsageEvent" },
      name: "cpu_time_ns",
      expr: "usage.user_cpu_ns + usage.system_cpu_ns",
      unit: "ns",
      metric: { kind: "counter" },
      label: "CPU time"
    }
  ],
  views: [
    {
      id: "process.cpu",
      title: "CPU Usage",
      sources: [
        { id: "usage", event: "ProcessResourceUsageEvent" }
      ],
      display: {
        kind: "time_series",
        y_min: 0,
        guides: [
          {
            kind: "horizontal_line",
            expr: "metadata['process.available_parallelism']",
            label: "available parallelism",
            unit: "cores"
          }
        ]
      },
      summary: [
        {
          label: "avg",
          expr: "aggregate('duration_weighted_mean', series['cores'], range['visible'], point.value)",
          unit: "cores"
        },
        {
          label: "avg",
          expr: "aggregate('duration_weighted_mean', series['cores'], range['visible'], point.value) / metadata['process.available_parallelism'] * 100",
          unit: "%"
        },
        {
          label: "max",
          expr: "aggregate('max', series['cores'], range['visible'], point.value)",
          unit: "cores"
        }
      ],
      series: [
        {
          id: "cores",
          title: "CPU",
          expr: "rate(usage.cpu_time_ns, usage.timestamp)",
          unit: "cores",
          mark: "step_area",
          metric: { kind: "gauge" },
          window: {
            missing_previous: "skip",
            on_decrease: "skip"
          },
          downsample: "max_per_pixel",
          tooltip: [
            { label: "Window", expr: "point.wall_delta_ns", unit: "ns" },
            { label: "CPU time", expr: "point.value_delta_ns", unit: "ns" },
            { label: "Cores", expr: "point.value" },
            {
              label: "Total CPU",
              expr: "point.value / metadata['process.available_parallelism'] * 100",
              unit: "%"
            }
          ]
        }
      ]
    }
  ]
}
```

Input events:

```js
usage[0] = { timestamp: 1000, user_cpu_ns: 100, system_cpu_ns: 50 };
usage[1] = { timestamp: 2000, user_cpu_ns: 500, system_cpu_ns: 150 };

// After computed_fields:
usage[0].computed = { cpu_time_ns: 150 };
usage[1].computed = { cpu_time_ns: 650 };
```

Output point:

```js
{
  start: 1000,
  end: 2000,
  t: 2000,
  value: 0.5,
  value_delta_ns: 500,
  wall_delta_ns: 1000,
  current: { usage: usage[1] },
  previous: { usage: usage[0] }
}
```

## Example: Socket Accept Queue Utilization

The examples below show individual view specs. They can be wrapped in the bundle `views` array when transmitted together with computed fields.

```js
{
  id: "socket.accept_queue",
  title: "Socket Accept Queue",
  sources: [
    { id: "accept", event: "TcpAcceptQueueEvent" }
  ],
  display: {
    kind: "time_series",
    y_min: 0,
    y_max: 100
  },
  series: [
    {
      id: "utilization",
      title: "Accept Queue Utilization",
      expr: "accept.pending_connections / accept.backlog_limit * 100",
      unit: "%",
      mark: "step_line",
      metric: { kind: "gauge" },
      partition_by: ["accept.socket_cookie"],
      thresholds: [
        { expr: 80, level: "warning" },
        { expr: 100, level: "critical" }
      ],
      downsample: "max_per_pixel",
      tooltip: [
        { label: "Socket", expr: "point.current.accept.socket_cookie" },
        { label: "Address", expr: "point.current.accept.local_addr" },
        { label: "Port", expr: "point.current.accept.local_port" },
        { label: "Pending", expr: "point.current.accept.pending_connections" },
        { label: "Backlog", expr: "point.current.accept.backlog_limit" },
        { label: "Utilization", expr: "point.value", unit: "%" }
      ]
    }
  ]
}
```

## Example: Queue Depth As Multi-Series

This is not first-iteration scope, but the spec shape should not block it.

```js
{
  id: "queue.depth",
  title: "Queue Depth",
  sources: [
    { id: "queue", event: "QueueSampleEvent" },
    { id: "worker", event: "WorkerUnparkEvent" },
    { id: "spawn", event: "TaskSpawnEvent" },
    { id: "terminate", event: "TaskTerminateEvent" }
  ],
  display: { kind: "time_series" },
  series: [
    {
      id: "global",
      title: "Global",
      expr: "queue.global_queue",
      unit: "tasks",
      mark: "step_area",
      metric: { kind: "gauge" },
      downsample: "max_per_pixel"
    },
    {
      id: "local",
      title: "Local",
      expr: "worker.local_queue",
      unit: "tasks",
      mark: "step_line",
      metric: { kind: "gauge" },
      partition_by: ["worker.worker_id"],
      downsample: "max_per_pixel"
    },
    {
      id: "active_tasks",
      title: "Active tasks",
      expr: "count_active(spawn.task_id, terminate.task_id)",
      unit: "tasks",
      mark: "step_line",
      metric: { kind: "up_down_counter" },
      downsample: "last_per_pixel"
    }
  ]
}
```

## Example: Spans As Interval Bars

```js
{
  id: "spans",
  title: "Spans",
  sources: [
    { id: "enter", event: "SpanEnterEvent" },
    { id: "exit", event: "SpanExitEvent" },
    { id: "close", event: "SpanCloseEvent" }
  ],
  transform: {
    op: "pair_intervals",
    start: "enter.timestamp",
    end: "exit.timestamp",
    key: "enter.span_id",
    close: "close.span_id"
  },
  display: {
    kind: "interval_bars",
    color_by: "duration"
  },
  tooltip: [
    { label: "Span", expr: "point.current.enter.span_name" },
    { label: "Duration", expr: "point.end - point.start", unit: "ns" },
    { label: "Worker", expr: "point.current.enter.worker_id" }
  ]
}
```

## Initial Work Split

### Frontend MVP

Implement first:

- Use existing custom events as the initial source model.
- Keep custom-event unit metadata available to computed fields and generic rendering.
- Add hardcoded JS spec bundles using the same shape expected from future trace metadata.
- Implement `computed_fields` and expose them in generic/custom event detail rendering.
- Implement `buildTimeSeriesFromSpec(trace, spec)`.
- Implement first expression support: field access, metadata access, arithmetic, `rate(value, time)`, `partition_by`.
- Support scalar aggregation over resolved series for view summaries.
- Implement a generic time-series renderer over resolved views using existing `timePanelLayout`.
- Support `mark`: `step_line`, `step_area`, and `line`.
- Support `display.guides`, `tooltip`, `thresholds`, `downsample` for `time_series`.
- Migrate CPU Usage to the generic spec path.
- Use Socket Accept Queue and Context Switch Rate as additional validation cases.

Deliberately deferred:

- Specs are hardcoded in JS at first.
- A unified event source model that makes built-in and custom events available to specs; the first iteration resolves only existing custom events.
- Only `time_series` is implemented initially.
- Expression language is intentionally small.
- Multi-source execution and its join shape are not yet defined.
- Field-level metric semantics and monotonic counter validation; these should come from Rust/schema metadata rather than duplicated JavaScript declarations.
- Full validation and user-visible diagnostics for trace-provided specs.
- `interval_bars`, `markers`, `heatmap`, and `flamegraph` stay future work.

### Rust API And Transmission

Second phase:

- Decide where global view specs live in the trace format.
- Avoid overloading `FieldAnnotation` for schema-wide or trace-wide specs.
- Keep field annotations for field-local metadata such as `unit` and metric semantics.
- Add a schema-level or trace-level metadata mechanism for computed fields and view specs.
- Add Rust builders/macros only after the JSON shape is validated in JS.
- Generate specs as JSON or equivalent structured data from Rust.

Potential Rust ergonomics:

```rust
#[traceevent(computed(
    name = "cpu_time_ns",
    expr = "user_cpu_ns + system_cpu_ns",
    unit = "ns",
    metric = "counter"
))]
```

```rust
TraceSpecBundle::builder()
    .computed_field(
        ComputedFieldSpec::builder()
            .source("usage", "ProcessResourceUsageEvent")
            .name("cpu_time_ns")
            .expr("usage.user_cpu_ns + usage.system_cpu_ns")
            .unit("ns")
            .metric_kind(MetricKind::Counter)
            .build(),
    )
    .view(
        TraceViewSpec::builder()
            .id("process.cpu")
            .title("CPU Usage")
            .time_series()
            .source("usage", "ProcessResourceUsageEvent")
            .series(
                SeriesSpec::builder()
                    .id("cores")
                    .expr("rate(usage.cpu_time_ns, usage.timestamp)")
                    .unit("cores")
                    .mark("step_area")
                    .metric_kind(MetricKind::Gauge)
                    .window(WindowSpec::builder().on_decrease(OnDecrease::Skip).build())
                    .build(),
            )
            .build(),
    )
    .build();
```

### Future Work

- Public expression syntax decision: JSON AST, `jsep` syntax, Rhai, or another engine.
- Multi-source joins and pairing.
- `interval_bars` generic renderer.
- Generic event marker panels.
- Heatmap encodings.
- Flamegraph specs.
- Cross-trace compatibility/versioning for specs.
- UI for hiding/reordering schema-recommended panels.
- Validation and diagnostics for invalid specs.

## Expression Engine Options

### JSON AST

Pros:

- Safe by construction.
- Easy to validate.
- No parser dependency.
- Can be produced by Rust builders.
- Good internal representation even if a string syntax is added later.

Cons:

- Verbose for humans.
- Less ergonomic in Rust attributes.

### `jsep`

Pros:

- Lightweight JS expression parser.
- Lets specs use readable strings.
- We still control evaluation and allowed operations.

Cons:

- Adds frontend dependency.
- Needs a custom evaluator and validation layer.
- JS-only unless Rust also implements/uses a compatible parser.

### Rhai

Pros:

- Rust-native scripting language.
- Supports expression-only mode.
- Has sandbox/resource controls.
- Can compile to WebAssembly.
- Could be reused if more viewer logic moves to Rust/WASM.

Cons:

- Adds WASM/runtime complexity to the current vanilla JS viewer.
- JS-to-WASM data transfer can be expensive if evaluated per event.
- Public syntax would become Rhai syntax.
- Needs performance and bundle-size investigation.

### Direct JavaScript

Pros:

- Very expressive.
- Easy to prototype.

Cons:

- Unsafe for trace-provided expressions.
- A trace can be downloaded from an untrusted source such as `https://evil.domain/demo-trace.bin`.
- Executing JS from trace metadata risks XSS, data exfiltration, storage access, network calls, or denial of service.
- Sandboxing via iframe or QuickJS/WASM is possible but adds substantial complexity.

Recommendation: start with JSON AST internally, optionally add `jsep` syntax later. Do not execute trace-provided JavaScript.

## External References

### Perfetto

Useful ideas:

- Separate raw trace ingestion from derived analysis.
- Derived metrics and debug tracks are query/spec driven.
- Trace Processor uses SQL to create reusable derived views and summaries.
- Structured queries keep scalar calculations and collection aggregation distinct.
- The viewer renders known generic track shapes rather than each producer owning rendering code.
- Counter descriptors separate standard units, custom unit names, and storage multipliers.
- Trace summaries carry standard units or a custom unit independently of display names.

Reference areas:

- Trace Processor
- Trace metrics
- Debug tracks

### OpenTelemetry

Useful ideas:

- Metric kinds: gauge, counter, up/down counter, histogram.
- Instrument units and semantic conventions.
- Distinction between metric meaning and export/rendering.
- Temporality vocabulary for counters: cumulative vs delta.

### Grafana

Useful ideas:

- Panels are configured separately from data queries.
- Transformations derive display data before visualization.
- Field options include units, thresholds, mappings, min/max, and display formatting.

### Vega-Lite

Useful ideas:

- Declarative visualization grammar.
- Transforms such as calculate, aggregate, and window/lag.
- Marks and encodings are separate from data transforms.
