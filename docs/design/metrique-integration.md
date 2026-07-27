# Metrique integration

> **Status: implemented.** The sink is the `dial9-metrique` crate, re-exported as `dial9::metrique_sink` behind the `metrique-sink` feature. The crate is tokio-free; its `tokio` feature adds task-id capture and is enabled by the facade when dial9's `tokio` feature is also on. This document records the design; for API details, see the crate rustdoc (`dial9_metrique`), which is kept authoritative.

Dial9 is a peer metrique sink. Users configure dial9 alongside their existing EMF/JSON metrique pipeline; every metrique entry that flows through the configured sink is also recorded into the dial9 trace, so application metrics and runtime telemetry share one file.

The sink reads metrique's entry descriptor for each entry to learn its structural shape (fields, flags, units), identifies caller-thread context via a sink-internal field flag on flattened context fields, and encodes the user-selected subset of fields into the dial9 trace. Nothing about the integration requires a dial9-specific metrique macro or dial9-specific newtype wrappers on fields.

The metrique side is the entry descriptor and field flag system (`docs/entry-descriptors.md` in the metrique repo).

## Glossary

- **`Dial9Stream`**: the dial9 `EntryIoStream` implementation. Composed into a user's metrique pipeline via `Dial9Stream::tee(&handle, emf)`. Consumes every entry that flows through the pipeline and encodes dial9-opted entries into the trace.
- **`WithoutDial9Fields`**: `EntryIoStream` wrapper that hides `dial9.`-prefixed fields from the sink it wraps. `Dial9Stream::tee` puts it around the non-dial9 side so the trace gets the runtime context and the EMF/JSON output does not.
- **`Dial9Context`**: metrique subfield users include in their entries to capture per-request runtime context (see its rustdoc).
- **`Dial9Event<E>`**: hand-written wrapper entry that contributes the same context fields around an existing entry, for callers who cannot or would rather not add a field. `Dial9EntryExt::append_on_drop_dial9` is the ergonomic entry point.
- **`Skip`** / **`Interned`**: the user-facing field flags for excluding a field from the payload and for string pooling (see their rustdoc).
- **`Context`**: crate-internal field flag carried by `Dial9Context`'s own fields; the sink discovers context fields by it when walking descriptors, and their presence is what opts an entry in. Would be replaced by a typed source-extraction mechanism in metrique.
- **Encode plan**: the cached per-entry-type routing table (`metrique_sink/plan.rs`). Built once per distinct descriptor-id sequence: wire schema with unit annotations, and an action (header / payload / skip) per field position.
- **Trace format**: dial9's wire format (`dial9-trace-format/SPEC.md`). The integration uses the schema-annotations frame (`TAG_SCHEMA_ANNOTATIONS`) for units and the self-describing `DynamicList` field type for list-shaped fields.

## User-facing API

See the `dial9_metrique` crate docs for the opt-in model and worked example.

There are two ways to include the context, both producing the same event: flatten a `Dial9Context` field into the entry, or wrap the entry with `Dial9Event` (via `append_on_drop_dial9`). The wrapper exists because adding a field is intrusive for shared or externally-owned metrics structs, and because a `#[cfg]` around one call site is easier to maintain than one around a struct field.

`Dial9Event` is hand-written because the macro does not emit the `where` clause a generic flattened field needs. It implements `InflectableEntry` at the default style only, which is enough for a root entry, and writes the wrapped entry before the context so the event is named after the entry rather than the context.

Convenience wiring (`ServiceMetrics::attach_to_stream_with_dial9`-style extension traits, a `metrique_sink(...)` builder) is deliberately **later scope**: it drags `metrique-service-metrics` into the public API surface. `Dial9Stream::tee(&handle, other)` is the supported path: it composes the two sinks and filters dial9's fields out of `other` in one call, without naming any service-metrics type.

### Keeping dial9's fields out of the other formats

`Dial9Context`'s fields are ordinary metrique fields, so they reach every sink in the pipeline. The monotonic timestamps are useless in EMF, and the thread/task ids are noise there too. Metrique has no per-format field exclusion, so `WithoutDial9Fields` wraps the other sink and drops `dial9.`-prefixed fields as they arrive: values are filtered in the `EntryWriter`, and the context's descriptor segment is dropped alongside them so a descriptor-aware downstream sink still sees a descriptor that matches the values it received. Dropping whole segments is enough because the context is always its own segment, whether flattened or wrapped. (A segment that mixes dial9-named and user fields, which takes a user field literally named `dial9.*`, cannot be subset out of `&'static` descriptor storage, so it degrades to `Descriptors::Unavailable` instead of reporting fields that no longer match the values.)

A general `drop_fields` in metrique itself would be the better long-term home, per review discussion.

## Architecture

Compile time: the metrique macro generates the entry descriptor (fields, flags, units, canonical name) alongside the existing `Entry` impl. Dial9 contributes only the three flag marker types and `Dial9Context`.

Caller thread: `Dial9Context::capture()` reads the OS thread id, `tokio::task::try_id()` (with the `tokio` feature), and the monotonic clock. Entry close records the end timestamp. The entry is queued as usual; dial9 adds no other request-path work.

Flush thread (whatever thread drives the metrique pipeline, e.g. `BackgroundQueue`'s): `Dial9Stream::next` per entry:

1. Disabled handle: return immediately (the tee still delivers entries to the other formats).
2. `entry.descriptors()`: `Unavailable` (hand-written entries that do not implement `descriptors()`, or entries containing `Flex` dynamic-key fields, whose descriptors are unavailable by construction) is skipped with a rate-limited warning.
3. Plan lookup keyed on the descriptor-id sequence; first use walks the descriptor segments and builds the plan (schema registration, unit annotations, diagnostics).
4. Walk `entry.write` with a capturing `EntryWriter` that routes each value callback per the plan, then assemble and encode the event.

### Value routing is positional

Metrique guarantees that walking `descriptors()` segments in sequence yields fields in exactly the order `Entry::write` emits `EntryWriter::value` callbacks, with each segment covering a contiguous slice of the write output (`docs/entry-descriptors.md`; for entries with interleaved flatten sites this means one descriptor segment per contiguous field run). The plan is therefore a flat action list in descriptor order, and the walk consumes it by callback index.

Counting the callbacks is a cheap defensive guard, so the sink does it: if the count differs from the plan in either direction the event is dropped with a rate-limited warning rather than recording mis-attributed values.

Field names still matter for the wire schema: two payload fields that emit the same post-rename name cannot share a schema, so the first occurrence keeps the name and later ones are skipped with a once-per-type diagnostic. Prefixing the flatten site (`#[metrics(flatten, prefix = "...")]`) is the documented remedy. Dial9's own header fields are outside this hazard entirely, being `dial9.`-prefixed.

`Dial9Context`'s fields use literal `#[metrics(name = "dial9.<field>")]` names rather than a prefix at the flatten site. Literal names are identical under every `NameStyle`, so a parent's `rename_all` cannot restyle them: the sink matches roles by exact base name, and the wire header names stay stable across entries that style their own fields differently.

### Event layout

One schema per distinct descriptor-id sequence, named `metrique:<EntryName>` (a `#<layout hash>` suffix disambiguates canonical-name collisions). The implicit event timestamp is `dial9.monotonic_ns_start` (flush-thread clock as fallback). Schema fields: `dial9.thread_id` (OS thread id, the same id space worker and CPU-sample events record), `dial9.task_id` (with the `tokio` feature), `dial9.duration_ns` (absent unless the context captured both timestamps; durations varint-encode in a fraction of the bytes an absolute end timestamp takes), `dial9.wall_clock_ns` (from `#[metrics(timestamp)]`, if any), then one field per supported descriptor field that is not flagged `Skip`. Every dial9-owned name, on the wire and in the other formats, carries the `dial9.` prefix, so it cannot collide with a user field and is trivially filterable.

Wire types come from the descriptor's `FieldShape`: unsigned widths map to `Varint` (dial9's `FieldValue` carrier for scalar integers; the fixed-width wire types would not match its encoding), signed to `I64`, floats to `F64`, `bool` to `Bool`, strings to `String` or `PooledString` per the `Interned` flag, with `Optional` variants for optional shapes. List shapes (`Vec<T>`, slices) map to the self-describing `DynamicList` type; elements are captured through the `values()` value callback and encode with their own scalar tags, so an `Interned` list of strings pools each element. Absent optional elements are omitted from the encoded list, the same way metrique's other formats leave them out of their arrays.

### Units

The descriptor carries `Option<Unit>` per field, resolved by metrique from the `Value` trait or an explicit `#[metrics(unit = ..)]`. Dial9 emits units as schema-level annotations with key `"unit"`, normalized to the vocabulary the viewer formats (`us`/`ms`/`s`/`bytes`, plus plain `count`); units outside it keep their CloudWatch name and render unformatted.

### Observability

Aggregate counters (plans built, events emitted, entries skipped/dropped) are reported at `debug` level every 60 s. Structural diagnostics fire once per entry type at plan build; per-entry failures are rate-limited. See the validation table below.

## Validation

The metrique macro catches intrinsic structural mistakes at compile time (conflicting flags). Dial9-specific diagnostics are runtime because the macro does not interpret flag identity.

First-use, per descriptor-id sequence (cached, so at most once per type):

| Condition | Behaviour |
| --- | --- |
| `descriptors()` unavailable (hand-written entry without a `descriptors()` impl, or `Flex` anywhere in the entry) | rate-limited warn; skipped on the dial9 side only |
| No `Context`-flagged fields (entry never opted in) | silently inert: the plan records nothing and `Entry::write` is not walked for entries of this type |
| Payload field name collides with an earlier payload field | one `tracing::error!` per type; the later occurrence is skipped. Header fields cannot be collided with: they are `dial9.`-prefixed |
| Two `Dial9Context`s flattened into one entry | warn per duplicated context field at plan build; the first flatten site keeps the header slots |
| `Interned` on a shape with no string data | one `tracing::error!` per type; field skipped on the wire; rest of entry encodes |
| `Opaque` shape (histograms, custom `Value` without `SHAPE`) | one `tracing::debug!` per type; field left out of the payload; rest encodes. Expected under implicit opt-in, so not an error |
| Unsupported list element shape (nested lists, bytes, `Flex`, `Opaque`) | one `tracing::debug!` per type; field left out of the payload; rest encodes |

Per entry:

| Condition | Behaviour |
| --- | --- |
| Inert handle / disabled recording | fast-path return; no work |
| Value callback count differs from the descriptor (metrique descriptor/write contract violation) | event dropped, rate-limited warn |
| Required field produced no value or mismatched data (including numeric lists through metrique's boxed dyn bridge, which stringifies elements) | event dropped, rate-limited warn naming the field |
| Panic inside `Value::write` | caught; event dropped, rate-limited warn; no event bytes are written until the walk completes, so at most orphaned string-pool entries remain (harmless) |
| Encoder rejects the assembled event (validation failure) | event dropped; dial9-core logs the reason |

## Performance

Measured by `dial9-metrique/benches/metrique_sink_bench.rs`; current numbers live in the module docs' Overhead section. On dial9's side, steady state allocates only for payload string and list values: routing tables, key lookups, and value buffers are cached or reused across entries. Metrique's `descriptors()` itself may allocate per entry once a layout exceeds its inline segment capacity; the precomputed sequence id ([awslabs/metrique#348](https://github.com/awslabs/metrique/issues/348)) removes that walk entirely.

## Future evolution

- **Viewer span rendering for metrique events.** Events carry start (`timestamp`) and `dial9.duration_ns`, so the span pipeline can synthesize a closed span per event (one segment, payload as span fields) with a small branch where span events are classified: `buildSpanData` in the legacy `trace_analysis.js` and its TypeScript port under `dial9-viewer/ui/src/lib/trace/`. Deferred until the UI migration (#672-674) leaves one implementation to change. Until then, metrique events appear in the events pane with fields and units.
- **A `Kpi` field flag** emitting a `dial9.kpi` annotation, once the viewer can graph flagged fields; the annotation mechanism needs no format changes.
- **Streaming event encoder** (transactional begin/commit/abort on the thread-local buffer), removing the buffered `FieldValue` stage here and the per-span allocations in the tracing layer.
- **Convenience wiring**: `attach_to_stream_with_dial9` / `metrique_sink(...)` builder, once we accept `metrique-service-metrics` in the public surface (later scope, see "User-facing API").
- **`Flex` support**, blocked on metrique giving `Flex` entries a self-describing descriptor shape (today `Flex::descriptors()` is `Unavailable`, which makes any entry containing one unavailable).
- **Numeric lists through `GlobalEntrySink`**, blocked on metrique's dyn bridge forwarding list elements without stringifying them ([awslabs/metrique#349](https://github.com/awslabs/metrique/issues/349)); until then only lists of strings work through a global sink, and entries with numeric list fields are dropped there.
- **Precomputed descriptor-sequence ids** (upstream), replacing the per-entry `descriptors()` walk and id hashing with a single `u64` read; identification is most of the gap between this sink and plain EMF formatting. Proposed in [awslabs/metrique#348](https://github.com/awslabs/metrique/issues/348).
- **A general `drop_fields` upstream in metrique**, replacing `WithoutDial9Fields` and letting a mixed segment be filtered properly instead of degrading to `Unavailable`.
- **More schema annotations**: display hints, aggregation hints, privacy labels, `dial9.kpi` markers. Same mechanism as units.
