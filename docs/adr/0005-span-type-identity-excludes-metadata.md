# Span type identity excludes metadata; grouping is a runtime operation

**Status:** accepted

A span type is identified by kind + callsite semantics only. Metadata such as a
poll's spawn location is **not** part of the type identity; instead the span
analysis machinery has a first-class **group-by-attribute** capability, and the
tokio view is "spans of kind `tokio_poll`, grouped by spawn location."

## Why

This **departs from the generalized-span design** (`docs/design/generalized-span-flamegraphs.md`
§1.1), which specifies the poll span type as "`tokio.poll` + spawn location" —
i.e. spawn location baked into `span_type_uid`. We chose runtime grouping
instead because:

- It generalizes: tracing spans can be grouped by any attribute (e.g.
  `handle_request` by `route`), not just polls by spawn location. Baking one
  attribute into identity does not.
- It matches the uniform in-code span model (ADR-forthcoming / CONTEXT "Span
  kind unification"): spawn location is a typed `derive(Deserialize)` field on
  the one span type, and grouping is a generic operation over metadata rather
  than baked identity.
- It keeps "a poll is a poll" — one `tokio_poll` type — so cross-kind analysis
  machinery (percentiles, histograms, composition, diff) treats every kind the
  same.

## Consequences

- `span_type_uid` for a poll is derived from kind + callsite, NOT spawn
  location. Code and any folded schema following §1.1's baked-in identity must
  be reconciled with this before the tokio view collapses into the span
  machinery.
- The span-stats query path needs a group-by-attribute capability it does not
  have today.
