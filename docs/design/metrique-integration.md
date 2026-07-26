# Metrique integration

> **Status: implemented.** The sink lives in `dial9-tokio-telemetry/src/metrique_sink/` behind the `metrique-sink` feature and is re-exported as `dial9::metrique_sink`. This document records the design; for API details, see the module rustdoc (`dial9_tokio_telemetry::metrique_sink`), which is kept authoritative.

Dial9 is a peer metrique sink. Users configure dial9 alongside their existing EMF/JSON metrique pipeline; every metrique entry that flows through the configured sink is also recorded into the dial9 trace, so application metrics and runtime telemetry share one file.

The sink reads metrique's entry descriptor for each entry to learn its structural shape (fields, flags, units), identifies caller-thread context via a sink-internal field flag on flattened context fields, and encodes the user-selected subset of fields into the dial9 trace. Nothing about the integration requires a dial9-specific metrique macro or dial9-specific newtype wrappers on fields.

The metrique side is the entry descriptor and field flag system (`docs/entry-descriptors.md` in the metrique repo).

## Glossary

- **`Dial9Stream`**: the dial9 `EntryIoStream` implementation. Composed into a user's metrique pipeline via `tee(emf, Dial9Stream::new(&handle))`. Consumes every entry that flows through the pipeline and encodes dial9-opted entries into the trace.
- **`Dial9Context`**: metrique subfield users flatten into their entries to capture per-request runtime context (see its rustdoc).
- **`Emit`** / **`Interned`**: the user-facing field flags for payload opt-in and string pooling (see their rustdoc).
- **`Context`**: crate-internal field flag carried by `Dial9Context`'s own fields; the sink discovers context fields by it when walking descriptors. Would be replaced by a typed source-extraction mechanism in metrique.
- **Encode plan**: the cached per-entry-type routing table (`metrique_sink/plan.rs`). Built once per distinct descriptor-id sequence: wire schema with unit annotations, and an action (header slot / payload slot / skip) per field position.
- **Trace format**: dial9's wire format (`dial9-trace-format/SPEC.md`). The integration uses the schema-annotations frame (`TAG_SCHEMA_ANNOTATIONS`) for units and the self-describing `DynamicList` field type for list-shaped fields.

## User-facing API

See the `dial9_tokio_telemetry::metrique_sink` module docs for the opt-in model and worked example.

Convenience wiring (`ServiceMetrics::attach_to_stream_with_dial9`-style extension traits, a `metrique_sink(...)` builder) is deliberately **later scope**: it drags `metrique-service-metrics` into the public API surface, and `tee` already composes with zero extra API. The `tee` primitive is the supported v1 path.

## Architecture

Compile time: the metrique macro generates the entry descriptor (fields, flags, units, canonical name) alongside the existing `Entry` impl. Dial9 contributes only the three flag marker types and `Dial9Context`.

Caller thread: `Dial9Context::capture()` reads the worker-id TLS, `tokio::task::try_id()`, and the monotonic clock. Entry close records the end timestamp. The entry is queued as usual; dial9 adds no other request-path work.

Flush thread (whatever thread drives the metrique pipeline, e.g. `BackgroundQueue`'s): `Dial9Stream::next` per entry:

1. Disabled handle: return immediately (the tee still delivers entries to the other formats).
2. `entry.descriptors()`: `Unavailable` (hand-written entries, or entries containing `Flex` dynamic-key fields, whose descriptors are unavailable by construction) is skipped with a rate-limited warning.
3. Plan lookup keyed on the descriptor-id sequence; first use walks the descriptor segments and builds the plan (schema registration, unit annotations, diagnostics).
4. Walk `entry.write` with a capturing `EntryWriter` that routes each value callback per the plan, then assemble and encode the event.

### Value routing is positional

Metrique guarantees that walking `descriptors()` segments in sequence yields fields in exactly the order `Entry::write` emits `EntryWriter::value` callbacks, with each segment covering a contiguous slice of the write output (`docs/entry-descriptors.md`; for entries with interleaved flatten sites this means one descriptor segment per contiguous field run). The plan is therefore a flat action list in descriptor order, and the walk consumes it by callback index.

A walk whose callback count differs from the plan in either direction means the entry violated that contract (an upstream metrique bug); the event is dropped with a rate-limited warning rather than recording mis-attributed values. Counting matters: a mid-walk omission shifts every later value one slot left, which type-compatible neighbors would otherwise absorb silently.

Field names still matter for the wire schema: two payload fields that emit the same post-rename name (or a payload field named like one of the reserved header fields) cannot share a schema, so the first occurrence keeps the name and later ones are skipped with a once-per-type diagnostic. Prefixing the flatten site (`#[metrics(flatten, prefix = "...")]`) is the documented remedy.

### Event layout

One schema per distinct descriptor-id sequence, named `metrique:<EntryName>` (a `#<layout hash>` suffix disambiguates canonical-name collisions). The implicit event timestamp is `monotonic_ns_start` (flush-thread clock as fallback). Schema fields: `worker_id`, `task_id`, `monotonic_ns_end`, `wall_clock_ns` (from `#[metrics(timestamp)]`, if any), then one field per supported `Emit`-tagged descriptor field.

Wire types come from the descriptor's `FieldShape`: unsigned widths map to `Varint` (dial9's `FieldValue` carrier for scalar integers; the fixed-width wire types would not match its encoding), signed to `I64`, floats to `F64`, `bool` to `Bool`, strings to `String` or `PooledString` per the `Interned` flag, with `Optional` variants for optional shapes. List shapes (`Vec<T>`, slices) map to the self-describing `DynamicList` type; elements are captured through the `values()` value callback and encode with their own scalar tags, so an `Interned` list of strings pools each element. Absent optional elements are omitted from the encoded list, the same way metrique's other formats leave them out of their arrays.

### Units

The descriptor carries `Option<Unit>` per field, resolved by metrique from the `Value` trait or an explicit `#[metrics(unit = ..)]`. Dial9 emits units as schema-level annotations with key `"unit"`, normalized to the vocabulary the viewer formats (`us`/`ms`/`s`/`bytes`, plus plain `count`); units outside it keep their CloudWatch name and render unformatted. The original design proposed a `"metrique.unit"` key, dropped because the viewer's unit handling is keyed on `"unit"`.

### Observability

Aggregate counters (plans built, events emitted, entries skipped/dropped) are reported at `debug` level every 60 s. Structural diagnostics fire once per entry type at plan build; per-entry failures are rate-limited. See the validation table below.

## Validation

The metrique macro catches intrinsic structural mistakes at compile time (conflicting flags). Dial9-specific diagnostics are runtime because the macro does not interpret flag identity.

First-use, per descriptor-id sequence (cached, so at most once per type):

| Condition | Behaviour |
| --- | --- |
| `descriptors()` unavailable (hand-written entry, or `Flex` anywhere in the entry) | rate-limited warn; skipped on the dial9 side only |
| `Emit` fields but no `Context`-flagged fields | one `tracing::warn!` per type; entries encode with `WorkerId::UNKNOWN` and flush-thread timestamp fallback |
| Payload field name collides with a header field (`worker_id`, ...) or an earlier payload field | one `tracing::error!` per type; the later occurrence is skipped |
| Two `Dial9Context`s flattened into one entry | warn per duplicated context field at plan build; the first flatten site keeps the header slots |
| `Interned` on a shape with no string data | one `tracing::error!` per type; field skipped on the wire; rest of entry encodes |
| `Opaque` shape (histograms, custom `Value` without `SHAPE`) tagged `Emit` | one `tracing::error!` per type; field skipped; rest encodes |
| Unsupported list element shape (nested lists, bytes, `Flex`, `Opaque`) | one `tracing::error!` per type; field skipped; rest encodes |

Per entry:

| Condition | Behaviour |
| --- | --- |
| Inert handle / disabled recording | fast-path return; no work |
| Value callback count differs from the descriptor (metrique descriptor/write contract violation) | event dropped, rate-limited warn |
| Required field produced no value or mismatched data (including numeric lists through metrique's boxed dyn bridge, which stringifies elements) | event dropped, rate-limited warn naming the field |
| Panic inside `Value::write` | caught; event dropped, rate-limited warn; no event bytes are written until the walk completes, so at most orphaned string-pool entries remain (harmless) |
| Encoder rejects the assembled event (validation failure) | event dropped; dial9-core logs the reason |

The original design called for `debug_assert!` on the no-context and Opaque cases. The implementation logs instead: a `#[cfg]`-gated context field is a legitimate configuration, and uniform debug/release behavior keeps the failure paths testable.

## Performance

Measured by `dial9-tokio-telemetry/benches/metrique_sink_bench.rs`; current numbers live in the module docs' Overhead section. Steady state allocates only for payload string and list values: routing tables, key lookups, and value buffers are cached or reused across entries.

## Future evolution

- **Viewer span rendering for metrique events.** Events carry start (`timestamp`) and end (`monotonic_ns_end`), so the span pipeline can synthesize a closed span per event (one segment, payload as span fields) with a small branch where span events are classified: `buildSpanData` in the legacy `trace_analysis.js` and its TypeScript port under `dial9-viewer/ui/src/lib/trace/`. Deferred until the UI migration (#672-674) leaves one implementation to change. Until then, metrique events appear in the events pane with fields and units.
- **A `Kpi` field flag** emitting a `dial9.kpi` annotation, once the viewer can graph flagged fields; the annotation mechanism needs no format changes.
- **Streaming event encoder** (transactional begin/commit/abort on the thread-local buffer), removing the buffered `FieldValue` stage here and the per-span allocations in the tracing layer.
- **Convenience wiring**: `attach_to_stream_with_dial9` / `metrique_sink(...)` builder, once we accept `metrique-service-metrics` in the public surface (later scope, see "User-facing API").
- **`Flex` support**, blocked on metrique giving `Flex` entries a self-describing descriptor shape (today `Flex::descriptors()` is `Unavailable`, which makes any entry containing one unavailable).
- **Numeric lists through `GlobalEntrySink`**, blocked on metrique's dyn bridge forwarding list elements without stringifying them; until then only lists of strings work through a global sink, and entries with numeric list fields are dropped there.
- **Hand-written `Entry` impls opting into descriptors** once metrique ships `DescribeEntry`.
- **Precomputed descriptor-sequence ids** (upstream), replacing the per-entry `descriptors()` walk and id hashing with a single `u64` read; identification is most of the gap between this sink and plain EMF formatting. Proposed in [awslabs/metrique#348](https://github.com/awslabs/metrique/issues/348).
- **Typed source extraction for context**, replacing flag-based `Dial9Context` discovery. This is also what would keep context fields out of the other formats: today `worker_id`/`task_id`/`monotonic_ns_start`/`monotonic_ns_end` travel as ordinary fields and appear in EMF/JSON output, and metrique has no per-format field exclusion to suppress them.
- **More schema annotations**: display hints, aggregation hints, privacy labels, `dial9.kpi` markers. Same mechanism as units.
- **Per-sink compile-time wire plans**, once metrique can emit them, replacing the flush-thread `Entry::write` walk entirely.
