# Generalized span flamegraphs

## Overview

Generalize dial9's poll-duration flamegraphs so any recorded span type can be
used as the unit of latency analysis. A Tokio `PollStart` → `PollEnd` interval is
one built-in span type; tracing spans provide application-defined types such as
`handle_request`, `query_database`, or `serialize_response`.

The aggregation pipeline remains demand-driven and source-file-bounded. It does
not attempt to reconstruct a globally complete span graph from a randomly
sampled, non-contiguous set of trace files. Instead, the producer emits a
self-contained summary when a tracing span closes. Folding one source file then
produces:

- one `spans/` row for every close summary in that file, including spans that
  began before the file;
- CPU and scheduler sample rows enriched with the locally-known enclosing span
  summaries, normally zero, one, or two memberships per sample;
- exact elapsed duration for every closed span, with an explicit indication of
  how much detailed execution data was available in that source file.

This deliberately accepts the trace rotation period `R` as the practical upper
bound for complete per-span flamegraph and wall-time detail. A span longer than
`R` still appears with its correct elapsed duration and final metadata when its
close event is sampled, but CPU/wait details from source files that were not
folded are reported as unknown rather than reconstructed heuristically.

## Goals

- Make polls one span type rather than a flamegraph-specific special case.
- Discover available span types and show occurrence-weighted duration
  histograms and percentiles.
- Brush a span-duration range and build flamegraphs from CPU samples enclosed by
  matching spans.
- Break elapsed time into estimated on-CPU, estimated synchronous blocking,
  async wait, Tokio scheduling delay, and unknown time.
- Keep queries simple: span flamegraphs read enriched sample rows without a
  temporal join or a globally materialized span graph.
- Include spans that have no CPU samples in discovery, histograms, percentiles,
  and wall-time summaries.
- Preserve demand-driven folding, deterministic per-source output, coverage,
  and samples-part completion semantics.
- Read old traces and tolerate old folded output through a format-version bump.

## Non-goals

- Joining arbitrary spans across non-contiguous source files.
- Recovering complete CPU or wait detail for spans longer than the folded source
  file.
- Treating missing CPU samples as exact proof of blocking.
- Inferring contextual parentage that the tracing layer did not record.
- Returning every span instance to the browser for fleet-sized scopes.
- Automatically exposing high-cardinality attributes such as request IDs as UI
  facets.
- Replacing the raw trace as the source of truth. Derived attribution may be
  recomputed by bumping the folded format version.

---

## 1. Span semantics

### 1.1 Span instance and span type

A **span instance** is one occurrence of an operation. A **span type** groups
instances with the same producer and callsite semantics.

Examples:

| Kind | Span type | Instance |
|---|---|---|
| Tokio | `tokio.poll` + spawn location | One task poll |
| Tracing | target + name + file + line | One tracing span lifecycle |

Every interval is half-open: `[start_ns, end_ns)`.

A poll has one execution interval and
`elapsed_ns == active_wall_ns == PollEnd - PollStart`.

A tracing span has a logical lifecycle and zero or more entered execution
intervals. An async instrumented operation can enter and exit once per poll and
can migrate workers. The following terms are intentionally distinct:

- **Elapsed time:** lifecycle close minus lifecycle start.
- **Active wall time:** wall-clock union of entered intervals observed in the
  folded source file.
- **Active thread time:** sum of entered intervals. This may exceed active wall
  time if the span is entered concurrently.
- **Detail coverage:** observed active/gap detail compared with elapsed time.

### 1.2 Source-file-bounded completeness

A close summary in source file `F` creates the authoritative span row for that
fold. Its elapsed duration is complete because the close event carries the
start timestamp. Its detailed attribution is complete only if all required
enter/exit, poll, wake, and profiler events are present in `F`.

The row records:

- `details_complete`: the lifecycle and all classified intervals fit within the
  source file;
- `detail_coverage_ns`: elapsed time for which the fold had sufficient local
  structural information;
- `unknown_ns`: all remaining elapsed time, including unsampled source-file
  history.

A span that starts in one source file and closes in another therefore appears
when the close file is folded. It has the true elapsed duration, but the missing
part is unknown. No neighboring source file is fetched implicitly.

### 1.3 Stable identity

`tracing::span::Id` values may be recycled. `Dial9TracingLayer` assigns each new
span a process-local monotonically increasing `span_instance_id`. The process
boot ID from the source path namespaces it across processes. Folded rows use:

```text
span_uid = BLAKE3(boot_id || span_instance_id)[0..16]
span_type_uid = BLAKE3(kind || target || name || file || line)[0..16]
```

The existing tracing ID remains on the wire for old viewer reconstruction, but
is not the durable folded identity. Explicit parent references use the stable
instance ID when available. A missing parent is represented as unknown, not as
an asserted root.

---

## 2. Producer events

### 2.1 Self-contained close summary

Extend `SpanCloseEvent` additively. Old traces omit the new fields; current
viewers ignore fields they do not use.

Required fields:

```text
timestamp_ns                 // close timestamp; existing event timestamp
span_id                      // existing tracing wire ID
span_instance_id
start_timestamp_ns           // lifecycle creation/start timestamp
first_enter_timestamp_ns?    // absent if never entered
active_ns                    // accumulated enter→exit thread time
span_name
target
file?
line?
parent_span_instance_id?
attributes: MAP<STRING, STRING>
```

The summary must be sufficient to write a useful `spans/` row even if no enter
event for that instance is present in the same source file.

The producer tracks `start_timestamp_ns` at `on_new_span`. It accumulates
`active_ns` across balanced enters/exits. Concurrent or re-entrant entries are
paired per thread; any unbalanced entries at close set a summary quality flag
rather than fabricating a duration.

Enter and exit events retain their existing schema for raw timeline rendering.
They additionally carry `span_instance_id` and `tid`, allowing the folder to
correlate local execution intervals without relying on worker identity during
`block_in_place` handoffs.

### 2.2 Effective profiler metadata

The fold needs the effective configuration, not an assumed default. Segment
metadata records at least:

```text
cpu.profile.enabled
cpu.profile.frequency_hz
cpu.profile.backend
cpu.profile.event_source
sched.profile.enabled
sched.profile.sample_interval
```

Relevant profiler or recorder drop counts must also be available. Attribution
is unknown when the effective configuration or loss state is unavailable.

---

## 3. Folded Parquet schema

Changing persisted content increments `SAMPLES_FORMAT_VERSION`. The first
implementation writes a new versioned tree and lazily refolds source files.
`ORDER_VERSION` does not change.

### 3.1 Spans table

Path:

```text
spans/service={service}/date={YYYY-MM-DD}/host={host}/{source_hash}.parquet
```

One row per span close summary in the source file, plus built-in poll rows when
the generalized endpoint requests the poll adapter. The existing `polls/`
projection remains initially for `/api/tokio-stats`; duplicating all polls into
`spans/` is deferred until storage measurements justify replacing it.

| Column | Type | Nullable | Meaning |
|---|---|---:|---|
| `span_uid` | `FIXED_BINARY(16)` | no | Stable instance identity |
| `span_type_uid` | `FIXED_BINARY(16)` | no | Grouping identity |
| `kind` | `STRING` | no | `tracing` or `tokio_poll` |
| `name` | `STRING` | no | Display name |
| `target` | `STRING` | yes | Tracing target |
| `callsite_file` | `STRING` | yes | Source file |
| `callsite_line` | `UINT32` | yes | Source line |
| `start_ns` | `INT64` | no | Lifecycle start |
| `end_ns` | `INT64` | no | Lifecycle close |
| `elapsed_ns` | `INT64` | no | `end_ns - start_ns` |
| `active_ns` | `INT64` | yes | Producer-accumulated active thread time |
| `observed_active_wall_ns` | `INT64` | no | Union of local entered intervals |
| `detail_coverage_ns` | `INT64` | no | Locally classifiable elapsed time |
| `details_complete` | `BOOL` | no | Complete detail fits in this fold |
| `concurrent` | `BOOL` | no | Concurrent/re-entrant execution observed |
| `parent_span_uid` | `FIXED_BINARY(16)` | yes | Explicit stable parent |
| `attributes` | `MAP<STRING,STRING>` | no | Final close-time attributes |
| `on_cpu_ns_est` | `INT64` | yes | Sampling estimate |
| `blocked_ns_est` | `INT64` | yes | Sampling residual estimate |
| `async_wait_ns` | `INT64` | yes | Not-ready time when task-scoped |
| `scheduler_delay_ns` | `INT64` | yes | Ready-to-next-poll time |
| `unknown_ns` | `INT64` | no | Unclassified elapsed time |
| `cpu_sample_count` | `UINT32` | no | Raw local CPU samples |
| `sched_sample_count` | `UINT32` | no | Raw local sched samples |
| `attribution_version` | `UINT16` | no | Derivation algorithm |
| `attribution_flags` | `UINT32` | no | Missing/lost/ambiguous inputs |
| `source_key` | `STRING` | no | Origin source file |
| `host/service/date` | `STRING` | no | Scope columns |

The five time categories plus `unknown_ns` are top-level fields for simple
aggregation. They are derived caches; raw trace events remain authoritative.

### 3.2 OpenTelemetry Arrow alignment

The folded schema should align with the current OpenTelemetry Arrow Protocol
(OTAP) where the domain semantics match, without claiming byte-for-byte OTAP
conformance. OTAP is an experimental Arrow IPC transport and in-memory model;
dial9 stores a source-file-bounded Parquet analysis cache with additional
profiling facts.

OTAP represents traces as a normalized star schema: `SPANS`, `SPAN_ATTRS`,
`SPAN_EVENTS`, `SPAN_LINKS`, and their attribute tables. It deliberately avoids
putting a complete span hierarchy in a nested `List<Struct>` so each relation can
be sorted, compressed, projected, and joined independently. Dial9 follows these
principles:

- use OTLP/OTAP field names and widths when the producer has those semantics:
  `trace_id` (`FIXED_BINARY(16)`), `span_id` (`FIXED_BINARY(8)`),
  `parent_span_id` (`FIXED_BINARY(8)`), `trace_state`, `name`, `kind`, start
  timestamp, duration, status, and dropped counts;
- keep `span_uid` and `span_type_uid` as explicitly dial9-local identities rather
  than pretending process-local tracing spans are distributed OTLP traces;
- reserve nullable OTLP identity/status columns so an OpenTelemetry-integrated
  producer can populate them without another schema redesign;
- keep dial9 profiling columns (`on_cpu_ns_est`, `async_wait_ns`, coverage,
  attribution flags, and so on) in the main span analysis table with a clear
  `dial9` meaning;
- preserve current tracing field fidelity as `MAP<STRING,STRING>` initially.
  OTAP's typed attribute relation (`key`, type tag, and nullable
  string/integer/double/bool/bytes/serialized value columns) is the evolution
  path when the producer preserves native field types or OTAP export is added.

The design does not adopt OTAP's batch-local `u16` row IDs as durable span
identity, nor its stateful IPC dictionary protocol. Parquet dictionary encoding
and dial9's stable source-file keys serve different purposes.

Authoritative references:

- <https://github.com/open-telemetry/otel-arrow/blob/main/docs/data_model.md>
- <https://github.com/open-telemetry/otel-arrow/blob/main/docs/otap_basics.md>
- <https://github.com/open-telemetry/opentelemetry-specification/blob/main/oteps/0156-columnar-encoding.md>

### 3.3 Enriched samples table

Keep all existing sample columns and add only a compact relationship:

```text
enclosing_spans: LIST<STRUCT<
    span_uid: FIXED_BINARY(16),
    span_type_uid: FIXED_BINARY(16),
    elapsed_ns: INT64,
    details_complete: BOOL
>>
```

Expected membership depth is zero, one, or two for most samples. Full names,
attributes, wall-time composition, and quality live only in `spans/`; they are
not duplicated into every sample. The small `elapsed_ns` and completeness cache
keeps the common type-plus-duration flamegraph filter to one sample scan.
Attribute and quality flamegraph filters are **deferred** (see §5.2); when
implemented they will select matching `span_uid`s from the spans part and then
test the compact membership list.

Only memberships proven by locally observed entered execution intervals are
attached. Lifecycle containment is not execution containment: samples while an
async span is exited/waiting must not be attached. A long span may have a
complete close-summary row in `spans/` but partial or absent memberships for
samples from other source files. This limitation is explicit in
`details_complete` and coverage.

For a sample enclosed by multiple selected spans, the flamegraph counts the raw
sample once. Inclusive per-span summaries may count that sample once for each
span instance; the UI labels such values inclusive.

### 3.4 Fold commit ordering

Write dictionaries, `polls/`, and `spans/` before `samples/`. Write `samples/`
last, including an empty part for a source file with no CPU samples. Its presence
remains the sole folded-set/completion record required by ADR-0003.

---

## 4. Wall-clock attribution

Attribution partitions elapsed time without silently converting uncertainty to
zero.

### 4.1 Active intervals

For locally observed entered intervals:

```text
sample_period_ns = 1_000_000_000 / effective_frequency_hz
on_cpu_ns_est = cpu_sample_count * sample_period_ns
expected_samples = observed_active_wall_ns * frequency_hz / 1_000_000_000
```

Clamp display estimates to the observed interval while retaining raw counts.
When profiler metadata is missing, drops are detected, worker/tid attribution is
ambiguous, or the interval is too short for useful inference, put the residual
in `unknown_ns`.

A starting confidence rule is three expected samples. With zero observed samples
and three expected samples, a continuously on-CPU interval would produce zero
with probability approximately `e^-3`, or 5%. This remains an estimate and is
shown as such.

Scheduler samples identify blocking stacks but do not measure blocked duration.
`blocked_ns_est` is the estimated active-wall residual, not scheduler sample
count multiplied by a period.

### 4.2 Inactive gaps

Split a gap between entered intervals only when all intervals correlate
unambiguously to one Tokio task and align with that task's polls:

```text
previous exit ── async wait ── effective wake ── scheduling delay ── next poll
```

- `async_wait_ns`: task returned `Pending` and was not ready.
- `scheduler_delay_ns`: task was ready but not being polled.
- missing/ambiguous wake or task correlation: unknown.

For generic nested or manually entered spans, inactivity may simply mean the
surrounding code ran outside the span. It is unknown, not async wait.

The UI label is **Async wait (not ready)** rather than merely **Idle**, avoiding
confusion with an idle Tokio worker.

### 4.3 Accounting invariant

For every span row:

```text
elapsed_ns = on_cpu_ns_est
           + blocked_ns_est
           + async_wait_ns
           + scheduler_delay_ns
           + unknown_ns
```

Nullable estimates contribute zero only when the corresponding quality flag
says the category was evaluated and absent. Otherwise their time remains in
`unknown_ns`.

---

## 5. Backend and query interface

### 5.1 Span statistics

Add `GET /api/span-stats` as an SSE refinement stream using the existing scope,
order key, sampling cap, fold limits, and coverage model.

Return bounded summaries grouped by `span_type_uid`:

- kind, name, and callsite;
- occurrence count;
- fixed log-duration histogram;
- p50/p95/p99/max;
- summed five-way wall-time composition;
- bounded low-cardinality attribute facets;
- partial/quality counts;
- bounded slow exemplars with viewer deep links.

Do not return every instance for fleet scopes. Attribute values become automatic
facets only below a configured cardinality cap; high-cardinality keys permit an
exact-value filter without listing all values.

### 5.2 Span-filtered flamegraphs

Extend `/api/flamegraph` with:

```text
span_type_uid
min_span_ns
max_span_ns
phase=on_cpu|blocking
```

> **Not yet implemented:** `span_attribute.<key>=<value>` filtering is designed
> but not yet supported by the backend. The query parameter is not accepted;
> passing it will be silently ignored by the URL parser (it is not a declared
> field in `FlamegraphParams`). Attribute-based flamegraph filtering requires a
> two-pass pipeline: first select matching `span_uid`s from the `spans/` table,
> then test the compact membership list in `samples/`. This will be added in a
> future iteration when the span explorer UI needs attribute drill-down.

The accumulator scans `samples/` and tests each row's short
`enclosing_spans` list. A row matches when any membership satisfies the type
and duration filters. The sample contributes once even
if multiple memberships match. Completeness (`details_complete`) filtering is
**deferred** alongside attribute filtering above.

`min_poll_ns` and `max_poll_ns` remain a compatibility adapter for the built-in
`tokio.poll` type until the existing poll UI migrates.

---

## 6. Span Explorer UI

Add a Span Explorer that uses `/api/span-stats` for discovery and the existing
flamegraph renderer for selected samples.

### 6.1 Catalog

One bounded row per available span type:

```text
Span | Kind | Instances | p50 | p95 | p99 | Time composition | Quality
```

Polls appear as the built-in `tokio.poll` type, optionally grouped by spawn
location. Tracing rows use name plus callsite to distinguish equal names.

### 6.2 Histogram and selection

The default histogram is occurrence-weighted: each span instance contributes
once. A toggle exposes CPU-sample weighting to preserve the current poll
minimap's meaning. Brushing sets the span-duration band and updates the selected
count, percentiles, wall-time composition, flamegraphs, and exemplars.

### 6.3 Detail tabs

- **On CPU:** source-0 samples in matching memberships.
- **Blocking:** scheduler/context-switch stacks in matching active intervals.
- **Time breakdown:** five-way composition by duration bucket.
- **Exemplars:** slow spans with host/time/source-file links.

Async wait and scheduling delay are durations, not CPU flamegraphs. Their detail
views show wake/task context when available. Partial and low-confidence data use
an explicit gray/hatched Unknown category; absent data is never rendered as a
plausible zero.

---

## 7. Compatibility and failure behavior

- Adding fields to self-describing trace events is wire-compatible. JavaScript
  access to new fields must tolerate `undefined` for old traces.
- Old traces without close summaries do not produce generalized tracing-span
  rows in the folded backend; exact client-side span rendering continues to use
  existing enter/exit reconstruction.
- Increment `SAMPLES_FORMAT_VERSION` so previously folded v3 source files are
  lazily refolded. Do not add side tables under v3: existing `samples/` leaves
  would incorrectly mark those sources complete.
- Malformed span events are counted and skipped with rate-limited logging. A
  failure to write `spans/` prevents the final `samples/` commit marker.
- Block-in-place gaps and unknown worker/tid attribution remain unknown per
  ADR-0002.
- A source file with no spans writes an empty `spans/` part and still commits its
  `samples/` part.

---

## 8. Delivery plan

1. **Producer summary:** stable instance IDs, self-contained close summaries,
   local active-time accumulation, tid on enter/exit, profiler metadata, and
   focused trace-format compatibility tests.
2. **Folded schema:** decode summaries and local segments, write `spans/`, enrich
   samples, preserve samples-last commit ordering, and bump the format version.
3. **Backend analysis:** span statistics SSE, occurrence histograms, bounded
   facets/exemplars, and generalized sample filtering.
4. **Span Explorer:** catalog, histogram brushing, five-way breakdown, on-CPU
   and blocking views, coverage, and deep links.
5. **Migration:** adapt poll-duration controls to `tokio.poll`; retain `polls/`
   until measured evidence supports removing it.
6. **Validation:** regenerate the demo trace, run focused Rust/JS tests, full
   nextest, stress nextest, formatting, and clippy.

## Alternatives considered

### Global cross-file span reconstruction

Rejected. The demand-driven order intentionally samples non-contiguous source
files. Fetching neighbors or maintaining mutable global span state would make a
fold no longer an independent, deterministic operation.

### Span table plus temporal interval joins

Correct but not chosen initially. A normalized segment table would retain every
entered interval and avoid persisting sample membership. Every flamegraph query
would then read and temporally join another dataset. The compact membership list
keeps the hot sample filter simple without duplicating full spans, while the raw
trace permits a future format version to revisit this choice.

### Samples table only

Rejected. Spans with no CPU samples would disappear, occurrence histograms would
be biased toward CPU-heavy spans, and close-time duration/metadata would have no
canonical row.

### Four time columns without Unknown

Rejected. CPU sampling is statistical, scheduler samples do not encode duration,
and source-file-bounded analysis intentionally lacks some history. Forcing all
elapsed time into four confident categories would manufacture precision.
