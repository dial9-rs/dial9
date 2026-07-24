# Ad-Hoc Span Wrappers Design

Issue: [dial9-rs/dial9#498](https://github.com/dial9-rs/dial9/issues/498)

Status: **implemented.** The core wrappers live in
`dial9-tokio-telemetry/src/span.rs` and the tower layer in
`dial9-tokio-telemetry/src/span/tower.rs` (feature `tower`), with tests in
`dial9-tokio-telemetry/tests/span_wrappers.rs`. This document is the design
rationale; the code is the source of truth for the exact signatures.

Companion UX artifact: [`ad-hoc-span-wrappers-example.rs`](./ad-hoc-span-wrappers-example.rs).
A complete program written against the API, showing it in context on an
axum stack. Read that file first, then come back here for the mechanics.

## Motivation

`Dial9TracingLayer` works well when an application is already instrumented
with `tracing`, but it demands:

1. a `tracing-subscriber` registry wired the way the layer expects, which
   may conflict with how the application already configures logging, and
2. `tracing` instrumentation in the first place.

Many services have neither. This design adds ad-hoc wrappers (a manual
span guard, a future wrapper, and a tower layer) that emit span metadata
directly into the dial9 trace with no tracing subscriber and no `tracing`
dependency. The goal is turnkey: add one line where you care, spans appear
in the viewer.

## Non-goals

- Replacing `Dial9TracingLayer`. Applications already on `tracing` should
  keep using it; the two coexist in one process (see *Span ID space*).
- Context propagation / implicit parenting. Contextual parents are
  unreliable across tasks (see the comment in `tracing_layer.rs::on_enter`);
  the viewer infers nesting from timestamp containment. We only support
  *explicit* parenting, same as the tracing layer.
- Log-style events (one-shot points without duration). `record_event` with
  a custom `#[derive(TraceEvent)]` struct already covers that.

## Wire format: reuse the existing standardized span events

The viewer already recognizes span events *by schema-name prefix*
(`trace_analysis.js::buildSpanData`, lines 1252-1308): `SpanEnter:*` /
`SpanEnter__*`, `SpanExit:*` / `SpanExit__*`, `SpanClose__*` /
`SpanCloseEvent`, with base fields `worker_id`, `span_id`,
`parent_span_id`, `span_name` and arbitrary extra fields. The ad-hoc
wrappers emit exactly this format:

| event | schema name | fields |
|---|---|---|
| enter | `SpanEnter:adhoc::{file}:{line}[:{field-names}]` | base + user fields (`OptionalPooledString`) |
| exit | `SpanExit:adhoc::{file}:{line}[:{field-names}]` | base (minus `parent_span_id`) + user fields |
| close | `SpanCloseEvent` (existing struct) | `timestamp_ns`, `span_id` |

The schema id is the callsite (`{file}:{line}`), not the span name. The
name is `impl Into<String>` and may be dynamic, so it rides in the
`span_name` field (which is what the viewer reads, `buildSpanData:1261`),
never in the schema id. When user fields are present, the comma-joined
field-name list is appended to the id (`...:{line}:{a,b}`), so that a
callsite emitting different field sets — e.g. an exit carrying a late
field the enter did not — resolves distinct, consistent schema
definitions rather than colliding on one id. Keying the id on the callsite
(plus field-name list) keeps schema cardinality bounded by code, not data.

No viewer changes are required. Old viewers render new traces; new
viewers render old traces. This satisfies the issue's "standardized format
that can be automatically rendered as spans in the UI": the format already
exists, this design just gives it a second producer.

The `adhoc::` pseudo-target keeps ad-hoc schema ids disjoint from the
tracing layer's `{target}::{name}:{file}:{line}` ids. A real Rust module
path can't be the literal `adhoc` crate root; even on collision, schemas
only need to be *consistent*, not unique, so this is cosmetic.

## API surface

New module `dial9_tokio_telemetry::span`. The core (guard + future
wrapper) has zero new dependencies and is always compiled. The tower
layer sits behind a new `tower` feature (adds a `tower-service` /
`tower-layer` dep, the small trait crates, not `tower` itself).

```rust
// Core vocabulary. See the example program for usage in context.
pub fn span(name: impl Into<String>) -> Span;          // #[track_caller]

pub struct Span;                                        // description, not yet entered
impl Span {
    pub fn field(self, name: &'static str, value: impl fmt::Display) -> Span;
    pub fn parent(self, parent: SpanId) -> Span;        // explicit only
    pub fn id(&self) -> SpanId;                         // pre-allocated, for parenting
    pub fn entered(self) -> EnteredSpan;                // sync RAII guard
}

pub struct EnteredSpan;                                 // !Send
impl EnteredSpan {
    pub fn record(&mut self, name: &'static str, value: impl fmt::Display);
    pub fn exit(self);                                  // also on Drop
}

pub trait SpanFutureExt: Future + Sized {
    #[track_caller]                                     // stamps the shorthand's callsite
    fn in_span(self, span: impl Into<Span>) -> SpanFuture<Self>;
}
impl<F: Future> SpanFutureExt for F {}
impl From<&str> for Span;                               // `.in_span("name")` shorthand
impl From<String> for Span;

pub struct SpanFuture<F>;                               // Future, transparent output
impl<F: Future> SpanFuture<F> {
    pub fn handle(&self) -> SpanHandle;                 // record fields after the fact
}

#[derive(Clone)]
pub struct SpanHandle;                                  // cheap, Send
impl SpanHandle {
    pub fn record(&self, name: &'static str, value: impl fmt::Display);
    pub fn id(&self) -> SpanId;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SpanId;
impl SpanId {
    pub fn as_u64(self) -> u64;                         // raw wire id
}
```

Tower layer (feature `tower`), configured with a `bon` builder per repo
API guidelines (all fields private, non-required fields defaulted):

```rust
pub struct Dial9SpanLayer<Req, Res>;
// Dial9SpanLayer::builder()
//     .make_span(|req: &Req| Span)          // required
//     .on_response(|&SpanHandle, &Res|)     // optional
//     .build()
```

### Design decisions

**Field names are `&'static str`, values are `impl Display`.**
Names become wire-schema field names. A schema is cached per
`(callsite, field-name list)`, so names must be low-cardinality and are
almost always literals; `&'static str` makes the cheap path the obvious
one. Values are interned as `PooledString`s (same as the tracing layer),
so repeated values are cheap; `impl Display` accepts ints, status codes,
paths, etc. without `.to_string()` noise at every call site. This
deliberately deviates from the `impl Into<String>` builder guideline for
*names* only, for the schema-cardinality reason above.

**The sync guard emits one enter/exit pair; the future wrapper emits one
pair per poll.** The guard covers on-thread blocking work, so a single
segment is exact. A future's span would otherwise report the entire await
as active. Per-poll pairs match what `tracing::Instrument` + the tracing
layer produce today, carry per-segment worker attribution, and cost the
same ~300ns/pair. (The viewer also reconstructs active/idle from
task polls, so both shapes render correctly either way.)

**`EnteredSpan` is `!Send` and must not be held across `.await`.** Same
rule as `tracing`'s `EnteredSpan`, for the same reason: the exit event
must be emitted on the entering thread's stream with a timestamp ordered
after the enter. Holding it across an await would attribute other tasks'
work to the span. The async story is `in_span`, full stop. We cannot make
holding-across-await a compile error without a lint, but `!Send` catches
the spawn case, and docs + the example program model the correct split.

**`SpanClose` is emitted on drop of the wrapper/guard,** whether the
future completed or was cancelled. Close is what lets the viewer finalize
and recycle the span id; a cancelled request still renders as a span
ending at cancellation time.

**Late fields via `SpanHandle::record` ride the next exit event.** The
viewer already prefers non-empty exit fields over enter fields
(`buildSpanData:1288-1297`: a non-empty exit-field set replaces the enter
fields), so recording e.g. `status` after the response head is available
needs no new event type. `SpanHandle` holds
`Arc<Mutex<Vec<(&'static str, String)>>>`-grade state; recording off the
polling thread is allowed (it's picked up at the next poll's exit). A
field recorded after the future's final poll has no later exit to ride, so
it is dropped; record before or during the last poll (the tower layer's
`on_response` fires inside the poll that yields the response, so it is
always in time).

**Off-runtime threads are silently skipped**, identical to the tracing
layer: every emit checks `Dial9Handle::current().is_enabled()`. Wrappers
are always safe to leave in code paths that sometimes run outside a
dial9-traced runtime, and are no-ops (branch + return) when dial9 is
disabled.

## Span ID space

The tracing layer uses `tracing`'s subscriber-allocated `span::Id` (small
integers starting at 1, recycled on close). Ad-hoc spans allocate from a
process-global `AtomicU64` counter **with the top bit set**
(`id = (1 << 63) | next`), so the two producers can never collide when
both run in one process. The viewer keys spans by the id's string form and
already handles id recycling via `SpanClose`, so no viewer change is
needed. 2^63 ids does not wrap in practice (at 1B spans/sec: ~292 years).

## Schema caching

`span()` captures `std::panic::Location::caller()` via `#[track_caller]`,
so the callsite is the user's `span(...)` line. The `From<&str>` shorthand
*cannot* capture the caller: `.in_span("name")` reaches the `Span` through
the blanket `Into`, which is not `#[track_caller]`, so a location captured
inside `From::from` would point at library code, not the call site. The
`Span` therefore carries `Option<&'static Location>`: `span()` fills it,
`From<&str>` leaves it `None`, and `in_span` (itself `#[track_caller]`)
stamps its own caller when the location is still `None`. Both forms end up
with the correct user callsite.

The enter/exit schemas are cached in a global
`Mutex<HashMap<(&'static Location, FieldNames), CallsiteSchemas>>`,
mirroring the tracing layer's per-`Identifier` cache. Keying on the
field-name list as well as the location means a callsite that
conditionally adds a field gets two (stable) schemas rather than dropped
fields, and it is also how late fields work: an exit whose field set
differs from the enter's simply resolves a different (stable) schema.
Schema count stays bounded by code, not data.

## Tower layer semantics (v1)

- One span per `call`, created by `make_span(&req)` on the request path.
- `on_response` fires when the inner future yields `Ok(response)` (for
  HTTP: the response *head*), recording late fields via the span's handle.
- The response future does **not** reuse `SpanFuture`. It has to sequence
  enter → poll → `on_response` → exit → close within a single poll, because
  the final exit is the last one that can carry a late field. Wrapping
  `SpanFuture` from the outside would fire its exit and close before
  `on_response` ran, silently dropping the recorded fields. The two futures
  share the emit helpers on `SpanInner` instead.
- The span closes when the response future resolves. **Streaming bodies
  are not covered by the span** in v1. Wrapping the body to hold the span
  open until end-of-stream is a documented follow-up (requires an
  `http-body` dep and per-frame enter/exit policy decisions).
- `Req` / `Res` are fully generic; nothing in the layer depends on `http`.
  The example program shows it on an axum stack because that is the
  motivating case.

## Feature flags & semver

- Core `span` module: unconditional (no new deps). All new API, purely
  additive.
- `tower` feature: gates `Dial9SpanLayer` (+ `tower-layer` / `tower-service`
  deps). Layer config via `#[bon::builder]`, private fields, optional
  everything except `make_span`; future fields (classifier, body
  wrapping, sampling) are non-breaking additions.
- No trace-format changes: only new instances of the existing
  self-describing span schemas. Old JS viewers render new traces unchanged.

## Open questions for review

1. `field` value type: `impl Display` (shipped) vs. a `Value` enum that
   preserves ints on the wire (`Varint` instead of interned string). The
   tracing layer stringifies everything today; matching it keeps the
   decoder story uniform, but a typed `Value` would help future numeric
   aggregation in the viewer. Shipped as stringify-in-v1; typed values
   remain a compatible schema addition later.
2. Should `.in_span("name")` (the `From<&str>` shorthand) exist, or is the
   two-step `span("name")` always better? Shipped with both. Note the
   location caveat under *Schema caching*: the shorthand's callsite is
   stamped by `in_span`, not by `From`, so a bare `Span::from("x")` that is
   never passed to `in_span` has no location. Today that combination is
   unreachable (the only consumer of a `From`-built `Span` is `in_span`),
   but it is why `Span::location` is an `Option`.
3. Module name: `span` (shipped) vs `spans` vs re-exports at crate root.
4. Does the tower layer belong in this crate behind a feature (shipped),
   or in a separate `dial9-tower` crate? Feature keeps versioning simple;
   separate crate keeps the core dep-free even at compile time.
