# Single-event spans

> **Status:** accepted for implementation.

This document defines the minimal schema convention for representing a
completed span as one dial9 trace event. It is producer-independent: metrique
is the first adapter, not part of the decoding contract.

The trace wire format treats schema annotations as opaque key-value strings.
This convention is a Dial9 interpretation layered above that format. Rust
producers and consumers share its public constants through
`dial9_core::schema_extensions`.

## Motivation

The trace format is self-describing and already carries arbitrary per-field
annotations for metadata such as units. Span semantics belong in those
annotations rather than in event or field-name matching.

A completed span is emitted when the work closes. Its packed event timestamp
therefore represents the span end. Encoding the start as the packed timestamp
would move the delta-encoded timestamp stream backwards at close time, forcing
timestamp resets and destroying emission-order locality.

The second timing quantity a span needs (start or duration) rides in a normal
annotated field. Duration is preferred there: it is a small elapsed count
rather than a full monotonic-clock absolute, and being unsigned it cannot
express a start after the end, so a producer computing it from two clock reads
saturates a backwards clock to a 0 duration instead of dropping the span.

## Wire contract

A span has three timing quantities — start, duration, and end — related by
`end = start + duration`. A decoder needs any **two** of them; it derives the
third. The packed event timestamp always supplies the end (see Motivation), so
a schema declares one of the other two:

```text
dial9.role = span.start      # start; duration = end - start
dial9.role = span.duration   # duration; start = end - duration
```

A schema is a single-event span schema when exactly one field carries a
`span.start` **or** `span.duration` role (declaring both is redundant and
treated as an invalid span schema). The annotated field and the packed end
timestamp are unsigned; start and end are monotonic-clock values, duration is
an elapsed count. The annotated field's `unit` annotation declares its scale;
v1 supports `ns`, `us`, `ms`, and `s`. When the `unit` annotation is absent,
the scale defaults to `ns`.

Intervals are half-open:

```text
[start, end)
```

Conversion to nanoseconds and all timestamp arithmetic must be checked. A
`span.duration` of 0 is a valid instantaneous span; because duration is
unsigned it can never encode `start > end`, so producers that compute
duration from two clock reads saturate a non-monotonic result to 0 rather than
drop the event.

### Span name

At most one string or pooled-string field (or the optional variant of
either) may carry:

```text
dial9.role = span.name
```

Its non-empty value is the span's display name. If the role is absent, the
schema name is the span name. If the role is present but the event value is
absent or empty, the decoder also falls back to the schema name.

### Span type

The start/duration field may additionally carry:

```text
dial9.span.type = <producer family>
```

For metrique the value is `metrique`. This is the producer/instrumentation
family, corresponding to the decoded span kind. The schema name remains the
structural type discriminator.

If the annotation is absent, the decoded type is `single-event`.

### Execution context

These roles are integer fields and are optional (they need not be present;
they are not modeled as optional on the wire):

```text
dial9.role = thread_id
dial9.role = tokio.task_id
dial9.role = tokio.worker_id
```

Physical field names are not semantic: annotations are authoritative.
Dial9-owned producers should use `dial9.`-prefixed names to avoid collisions
with application payload fields. These values describe execution context
captured at span start. `thread_id` is an OS thread ID.

Tokio task ID is the preferred correlation key. A thread ID must be resolved
against the time-local `WorkerPark`/`WorkerUnpark` timeline; it must not be
treated as having one stable worker mapping for the whole trace. A supplied
worker ID must not override a known block-in-place gap.

### Attributes

Fields carrying the `span.start`/`span.duration` or execution-context roles
are not copied into the span's attributes. The `span.name` field is the
exception: it stays a normal attribute (with its original name, value, and
unit) in addition to supplying the display name. All other event fields remain
attributes with their original names, values, and units. Decoders may project
the field shapes supported by their existing span-attribute model; decoders
that expose typed attributes also preserve their unit annotations.

## Validation

Schema validation happens once per wire schema:

- no `span.start` and no `span.duration` role: ordinary custom event;
- both `span.start` and `span.duration` present, or a duplicate start,
  duration, name, or context role: invalid span schema;
- unsupported start/duration/name/context wire type: invalid span schema;
- unknown role: ignored for forward compatibility;
- identical duplicate annotations: idempotent.

An invalid span schema remains decodable as an ordinary custom event and is
diagnosed once. Per-event failures such as a missing start/duration value or a
conversion overflow skip only that span projection and use rate-limited
diagnostics on repeated paths. Decoders must not invent timestamps or
identifiers.

## Metrique adapter

`Dial9Context` continues to capture monotonic start and close timestamps plus
thread and Tokio task IDs. `Dial9Stream` emits:

- packed event timestamp: captured close timestamp (span end);
- `dial9.span.duration_ns`: `end - start` (saturating to 0 if the clock went
  backwards), annotated with `dial9.role=span.duration`,
  `dial9.span.type=metrique`, and `unit=ns`;
- `dial9.thread_id`: annotated with `dial9.role=thread_id`;
- `dial9.tokio.task_id`: annotated with `dial9.role=tokio.task_id`;
- metrique payload fields and their existing unit annotations.

Emitting duration rather than the absolute start keeps the second timing value
a small varint (elapsed nanoseconds) instead of a full monotonic-clock reading,
and makes a non-monotonic start unrepresentable rather than a drop condition.

The public metrique field flag `SpanName` selects a payload field for
`dial9.role=span.name`:

```rust,ignore
#[metrics(flags(dial9::Interned, dial9::SpanName))]
operation: &'static str,
```

`SpanName` is valid only on a scalar string field. A second `SpanName`, use on
an incompatible shape, or a colliding/skipped field is a best-effort hint that
cannot be honored: the field records as an ordinary payload field and the span
falls back to its schema name (the event is never dropped over it). Entries
without a `SpanName` field use their schema name.

The `metrique:` schema prefix may remain as producer namespacing, but no
decoder may use it to recognize spans.

## Decoder module

Both Rust and JavaScript expose the same conceptual interface:

```text
compile(schema) -> NotSpan | Layout | InvalidSchema
decode(layout, event) -> SingleEventSpan | InvalidEvent
```

The compiled layout stores field indices, units, span type, name source,
context sources, and attribute indices. Downstream Rust aggregation and both
JavaScript storage paths consume the normalized `SingleEventSpan`; they do
not inspect annotations or physical field names.

The implemented span decoding paths are:

1. legacy tracing enter/exit/close reconstruction;
2. generalized single-event spans.

Generalized multi-event spans are out of scope.

Previously emitted unannotated metrique events are intentionally not
recognized as spans.

## Delivery

PR 1 defines this specification and updates the metrique producer. PR 2
replaces the metrique-specific viewer implementation with the generalized
single-event span decoder. PR 2 is rebased onto `main` after PR 1 merges.
