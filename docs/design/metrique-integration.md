# Metrique integration

> **Status: implemented.** The sink lives in `dial9-tokio-telemetry/src/metrique_sink/` behind the `metrique-sink` feature and is re-exported as `dial9::metrique_sink`. This document records the design and the deltas discovered during implementation; for API details, see the module rustdoc (`dial9_tokio_telemetry::metrique_sink`), which is kept authoritative.

Dial9 is a peer metrique sink. Users configure dial9 alongside their existing EMF/JSON metrique pipeline; every metrique entry that flows through the configured sink is also recorded into the dial9 trace. A single trace file carries both tokio runtime telemetry and per-request application metrics.

The sink reads metrique's entry descriptor for each entry to learn its structural shape (fields, flags, units), identifies caller-thread context via a sink-internal field flag on flattened context fields, and encodes the user-selected subset of fields into the dial9 trace. Nothing about the integration requires a dial9-specific metrique macro or dial9-specific newtype wrappers on fields.

The metrique side is the entry descriptor and field flag system (`docs/entry-descriptors.md` in the metrique repo, shipped in metrique 0.1.27+ / metrique-writer-core 0.1.16+).

## Glossary

- **`Dial9Stream`**: the dial9 `EntryIoStream` implementation. Composed into a user's metrique pipeline via `tee(emf, Dial9Stream::new(&handle))`. Consumes every entry that flows through the pipeline and encodes dial9-opted entries into the trace.
- **`Dial9Context`**: a metrique subfield struct users flatten into their entries. `capture()` records caller-thread `worker_id`, `task_id`, and `monotonic_ns_start`; its close-time field records `monotonic_ns_end`. These fields are flagged `Context` internally so the sink routes them into the trace event header rather than the payload.
- **`Emit`**: the user-facing field flag that opts a field into the dial9 payload. Applied at struct scope via `#[metrics(default_flags(Emit))]` or at field scope via `#[metrics(flags(Emit))]`; inverted with `flags(skip(Emit))`.
- **`Interned`**: the user-facing field flag that routes string data in this field through dial9's string pool. Orthogonal to `Emit`.
- **`Context`**: a `#[doc(hidden)]` dial9-internal field flag carried by `Dial9Context`'s own fields. Users do not interact with it directly; the sink discovers context fields by walking the descriptor at first use. The flag identity is not a stable guarantee; a future typed source-extraction mechanism in metrique would replace this discovery.
- **Encode plan**: the cached per-entry-type routing table (`metrique_sink/plan.rs`). Built once per distinct descriptor-id sequence: wire schema with unit annotations, and an action (header slot / payload slot / skip) per field.
- **Trace format**: dial9's wire format (`dial9-trace-format/SPEC.md`). The integration uses the schema-annotations frame (`TAG_SCHEMA_ANNOTATIONS`) for units. The typed `DynamicList`/`DynamicMap` field types also exist on the wire but are not yet used by this sink (see "Deltas from the original design").

## User-facing API

See the `dial9_tokio_telemetry::metrique_sink` module docs for the worked example. In short:

- Flatten a `Dial9Context` into the entry (`Dial9Context::capture()` at the initializer site) so events carry worker, task, and start/end timestamps.
- Flag payload fields with `Emit` (usually `default_flags(Emit)` at struct scope, `flags(skip(Emit))` to opt out).
- Flag low-cardinality strings with `Interned`.
- Wire the sink as a peer of the existing stream: `tee(emf_stream, Dial9Stream::new(&handle))`.

Convenience wiring (`ServiceMetrics::attach_to_stream_with_dial9`-style extension traits, a `metrique_sink(...)` builder) is deliberately **later scope**: it drags `metrique-service-metrics` into the public API surface, and `tee` already composes with zero extra API. The `tee` primitive is the supported v1 path.

## Architecture

Compile time: the metrique macro generates the entry descriptor (fields, flags, units, canonical name) alongside the existing `Entry` impl. Dial9 contributes only the three flag marker types and `Dial9Context`.

Caller thread: `Dial9Context::capture()` reads the worker-id TLS, `tokio::task::try_id()`, and the monotonic clock (~26 ns). Entry close records the end timestamp. The entry is queued as usual; dial9 adds no other request-path work.

Flush thread (whatever thread drives the metrique pipeline, e.g. `BackgroundQueue`'s): `Dial9Stream::next` per entry:

1. Disabled handle: return immediately (~2 ns); entries still reach the other side of the tee.
2. `entry.descriptors()`: `Unavailable` (hand-written entries) is skipped with a rate-limited warning.
3. Plan lookup keyed on the descriptor-id sequence; first use walks the descriptor segments and builds the plan (schema registration, unit annotations, diagnostics).
4. Walk `entry.write` with a capturing `EntryWriter` that routes each value callback per the plan, then assemble and encode the event (~570 ns for a mid-size entry; see the module docs' Overhead section and `benches/metrique_sink_bench.rs`).

### Value routing is by name, then by cached position

The original design assumed `Entry::write` emits value callbacks in descriptor order. **It does not**: metrique emits flatten children inline at their declaration position, while `descriptors()` yields the parent's own fields as one segment followed by child segments. Positional alignment between the two is therefore impossible in general.

The sink instead routes the first entry of each type by field name (post-rename full name, as emitted at write time), records the actions in write order, and caches that positional dispatch on the plan. Subsequent entries index it directly; a callback-count check still rejects entries whose field set is dynamic (`Flex`). Write order is deterministic per entry type (generated code), so the cached order is stable.

Two consequences:

- An entry that declares the same post-rename field name twice cannot be routed and is dropped (reported once per type).
- A parent's `rename_all` restyles flattened child names (`worker_id` → `WorkerId`), so `Dial9Context` field discovery matches canonicalized names (separators stripped, lowercased); see `metrique_sink/context.rs`.

### Event layout

One schema per distinct descriptor-id sequence, named `metrique:<EntryName>` (a `#N` suffix disambiguates rare canonical-name collisions). The implicit event timestamp is `monotonic_ns_start` (flush-thread clock as fallback). Schema fields: `worker_id`, `task_id`, `monotonic_ns_end`, `wall_clock_ns` (from `#[metrics(timestamp)]`, if any), then one field per supported `Emit`-tagged descriptor field.

Wire types come from the descriptor's `FieldShape`: unsigned widths map to `Varint` (dial9's `FieldValue` carrier for scalar integers), signed to `I64`, floats to `F64`, `bool` to `Bool`, strings to `String` or `PooledString` per the `Interned` flag, with `Optional` variants for optional shapes.

### Units

The descriptor carries `Option<Unit>` per field, resolved by metrique from the `Value` trait or an explicit `#[metrics(unit = ..)]`. Dial9 emits units as schema-level annotations with key `"unit"` and the unit's CloudWatch name (e.g. `"Milliseconds"`) as the value. This is the same key the `TraceEvent` derive uses, so the viewer surfaces both through one path. (The original design proposed `"metrique.unit"`; the viewer only maps `"unit"` to its per-field units table, so the shipped key follows the existing convention.)

### Observability

Aggregate counters (plans built, events emitted, entries skipped/dropped) are reported at `debug` level every 60 s. Structural diagnostics fire once per entry type at plan build; per-entry failures are rate-limited. See the validation table below.

## Validation

The metrique macro catches intrinsic structural mistakes at compile time (conflicting flags). Dial9-specific diagnostics are runtime because the macro does not interpret flag identity.

First-use, per descriptor-id sequence (cached, so at most once per type):

| Condition | Behaviour |
| --- | --- |
| `descriptors()` unavailable (hand-written entry) | rate-limited warn; entry skipped; other formats unaffected |
| `Emit` fields but no `Context`-flagged fields | one `tracing::error!` per type; entries encode with `WorkerId::UNKNOWN` and flush-thread timestamp fallback |
| `Interned` on a non-string shape | one `tracing::error!` per type; field skipped on the wire; rest of entry encodes |
| `Opaque` shape (histograms, custom `Value` without `SHAPE`) tagged `Emit` | one `tracing::error!` per type; field skipped; rest encodes |
| Duplicate post-rename field name | one `tracing::error!` per type; entries of this type dropped |

Per entry:

| Condition | Behaviour |
| --- | --- |
| Inert handle / disabled recording | fast-path return; no work |
| Dynamic fields (`Flex`) or any name/count mismatch vs the descriptor | event dropped, rate-limited warn |
| Required field produced no value (shape/value mismatch) | event dropped, rate-limited warn naming the field |
| Panic inside `Value::write` | caught; event dropped, rate-limited warn; capture happens before any event bytes are written, so encoder state stays valid |

The original design called for `debug_assert!` on the no-context and Opaque cases. The implementation deliberately uses `error!` only: a `#[cfg]`-gated context field is a legitimate configuration, and uniform debug/release behavior keeps the failure paths testable.

## Deltas from the original design

Discovered during implementation, each a candidate upstream metrique issue:

1. **Write order ≠ descriptor order** for flattened entries (see "Value routing"). The descriptor docs' order contract does not hold across segments; routing is by name with cached positional dispatch as the workaround.
2. **Lists are carried as comma-joined strings, not typed `DynamicList`s.** `#[metrics(flags(..))]` wraps every flagged field in `ForceFlag`, whose write-path wrapper does not forward `ValueWriter::values()`; list data therefore always reaches the sink through metrique's default comma-joined `string()` fallback. The wire format's typed list support is ready; the sink switches over once metrique forwards `values()`.
3. **`Flex` is unsupported** (dropped with a diagnostic): shipped metrique models `Flex` as its own descriptor segment whose fields are dynamic, so callbacks cannot be matched against the static descriptor.
4. **`u8`/`u16`/`u32` shapes map to `Varint`**, not the fixed-width wire types: dial9's dynamic-value carrier (`FieldValue`) has a single unsigned-integer representation.

## Performance

Measured by `dial9-tokio-telemetry/benches/metrique_sink_bench.rs` (see the module docs for current numbers): ~26 ns request-path capture, ~570 ns flush-thread encode per entry (~100 ns of which is metrique's `descriptors()` call), ~2 ns disabled. Per-entry heap traffic is limited to payload string values; routing tables, key lookups, and value buffers are cached or reused across entries.

## Future evolution

- **Streaming event encoder** (transactional begin/commit/abort on the thread-local buffer), removing the buffered `FieldValue` stage here and the per-span allocations in the tracing layer. Drafted in `.kiro/issue-draft-streaming-encoder.md`.
- **Convenience wiring**: `attach_to_stream_with_dial9` / `metrique_sink(...)` builder, once we accept `metrique-service-metrics` in the public surface (later scope, see "User-facing API").
- **Typed list encoding**, blocked on metrique forwarding `values()` through `ForceFlag` (delta 2).
- **`Flex` support**, blocked on metrique giving `Flex` segments a self-describing descriptor shape (delta 3).
- **Hand-written `Entry` impls opting into descriptors** once metrique ships `DescribeEntry`.
- **Typed source extraction for context**, replacing flag-based `Dial9Context` discovery.
- **More schema annotations**: display hints, aggregation hints, privacy labels, `dial9.kpi` markers. Same mechanism as units.
- **Per-sink compile-time wire plans**, once metrique can emit them, replacing the flush-thread `Entry::write` walk entirely.
