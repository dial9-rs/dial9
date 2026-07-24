# Ad-Hoc Span Wrappers Design

Issue: [dial9-rs/dial9#498](https://github.com/dial9-rs/dial9/issues/498)

Status: **implemented.** The core lives in `dial9-tokio-telemetry/src/span.rs`
(+ `span/future.rs`, `span/wire.rs`) and the tower layer in
`dial9-tokio-telemetry/src/span/tower.rs` (feature `tower`), re-exported from
the `dial9` umbrella crate. Tests are in
`dial9-tokio-telemetry/tests/span_wrappers.rs`; a runnable example is
`dial9/examples/adhoc_spans.rs`. This document is the design rationale; the code
is the source of truth for the exact signatures.

Companion UX artifact: [`ad-hoc-span-wrappers-example.rs`](./ad-hoc-span-wrappers-example.rs).
A complete program written against the API, showing it in context on an axum
stack. Read that file first, then come back here for the mechanics.

## Motivation

`Dial9TracingLayer` works well when an application is already instrumented with
`tracing`, but it demands:

1. a `tracing-subscriber` registry wired the way the layer expects, which may
   conflict with how the application already configures logging, and
2. `tracing` instrumentation in the first place.

Many services have neither. This design adds ad-hoc wrappers (a `dial9_span!`
macro, a synchronous guard, a future combinator, and a tower layer) that emit
span metadata directly into the dial9 trace with no tracing subscriber and no
`tracing` dependency. The goal is turnkey: add one line where you care, spans
appear in the viewer.

## Non-goals

- Replacing `Dial9TracingLayer`. Applications already on `tracing` should keep
  using it; the two coexist in one process (see *Span ID space*).
- Context propagation / implicit parenting. Contextual parents are unreliable
  across tasks (see the comment in `tracing_layer.rs::on_enter`); the viewer
  infers nesting from timestamp containment. We only support *explicit*
  parenting, same as the tracing layer.
- Late / deferred fields. Fields are captured at the call site when the span is
  built (that is what lets each callsite have a compile-time schema — see below).
  A value only knowable later (e.g. an HTTP status) is recorded by building the
  span where that value is in scope, not by mutating a live span. This is a
  deliberate trade against the runtime-schema alternative.
- Log-style events (one-shot points without duration). `record_event` with a
  custom `#[derive(TraceEvent)]` struct already covers that.

## Wire format: reuse the existing standardized span events

The viewer already recognizes span events *by schema-name prefix*
(`trace_analysis.js::buildSpanData`): `SpanEnter:*`, `SpanExit:*`, and
`SpanCloseEvent`, with base fields `worker_id`, `span_id`, `parent_span_id`,
`span_name` and arbitrary extra fields. The ad-hoc wrappers emit exactly this
format:

| event | schema name | fields |
|---|---|---|
| enter | `SpanEnter:<file>:<line>:<col>` (macro) or `SpanEnter:adhoc::runtime` (name-only) | base + user fields |
| exit  | `SpanExit:<file>:<line>:<col>` (macro) or `SpanExit:adhoc::runtime` (name-only) | base (minus `parent_span_id`) + user fields |
| close | `SpanCloseEvent` (existing struct, shared with the tracing layer) | `timestamp_ns`, `span_id` |

The schema id is the callsite (`file:line:col`), not the span name. The name is
`impl Into<String>` and may be dynamic, so it rides in the `span_name` field
(which is what the viewer reads), never in the schema id. Keying the id on the
callsite keeps schema cardinality bounded by code, not data.

No viewer changes are required. Old viewers render new traces; new viewers
render old traces. This satisfies the issue's "standardized format that can be
automatically rendered as spans in the UI": the format already exists, this
design just gives it a second producer.

## Compile-time schemas: no runtime schema cache, no lock

The `tracing` layer builds a `Schema` at runtime and memoizes it in a
`Mutex<HashMap<callsite, Schema>>`, taking that lock on every enter/exit. The
ad-hoc wrappers avoid the map and the lock entirely by making the schema a
**compile-time artifact per callsite**.

`dial9_span!(...)` expands, at each call site, to a pair of generated
`#[derive(TraceEvent)]` structs — `SpanEnter:<file>:<line>:<col>` and the
matching `SpanExit` — whose fields are exactly the framework fields plus the
user fields written at that call site. The derive already produces the schema
for a type at compile time, so emitting a span is just encoding a typed event:
no runtime schema build, no `HashMap`, no lock on the hot path. Two call sites
with different field sets are two different generated types with two different
(stable) schema names, so the encoder never sees "same name, different
definition".

The span carries only `&'static SpanWire` — two function pointers to the
generated enter/exit writers — plus its id, name, parent, and the already
formatted field-value strings. Emitting bridges a writer fn into the normal
`record_event` path (so the enabled-check and flush accounting are unchanged).

### Enabling derive change: `#[traceevent(name = <expr>)]`

The generated schema name (`"SpanEnter:…"`) is not a valid Rust identifier and
must vary per call site, so the `TraceEvent` derive gained a struct-level
`name = <expr>` attribute that overrides the default (the struct name) with any
`&'static str` expression. The macro uses
`concat!("SpanEnter:", file!(), ":", line!(), ":", column!())`. Because the
override is evaluated at the derive site, `file!()`/`line!()`/`column!()`
resolve to the user's callsite. This is the only change outside the `span`
module and is purely additive.

## API surface

New module `dial9_tokio_telemetry::span`, re-exported as `dial9::span`; the
`dial9_span!` macro is exported at the crate root. The core (macro + guard +
future combinator) has zero new dependencies and is always compiled. The tower
layer sits behind a new `tower` feature (adds the small `tower-service` /
`tower-layer` trait crates, not `tower` itself).

```rust
// Build a span, capturing the callsite and fields. tracing-style syntax:
// bare / `%` Display, `?` Debug, `~` prefix stores a value inline (not pooled).
dial9_span!("db.load", order_id = id, path = %p, cfg = ?c, req = ~request_id) -> Dial9Span;

pub struct Dial9Span;                         // not Clone: one identity, one close
impl Dial9Span {
    pub fn new(name: impl Into<String>) -> Dial9Span;   // name-only, runtime name
    pub fn with_parent(self, parent: &Dial9Span) -> Dial9Span;  // explicit only
    pub fn with_parent_id(self, parent_span_id: u64) -> Dial9Span;
    pub fn id(&self) -> u64;                  // for parenting across a boundary
    pub fn enter(&self) -> Entered<'_>;       // sync RAII guard
}

pub struct Entered<'_>;                        // !Send; emits exit + close on drop

pub trait Instrument: Future + Sized {
    fn instrument(self, span: Dial9Span) -> Instrumented<Self>;
}
pub struct Instrumented<F>;                    // Future, transparent output
```

Tower layer (feature `tower`):

```rust
// Fixed name:
Dial9SpanLayer::named("http_request")
// Per-request span (attach fields via the macro):
Dial9SpanLayer::new(|| dial9_span!("rpc", service = "auth"))
```

### Design decisions

**Field names are macro identifiers; values are `Display`/`Debug`.** Names
become wire-schema field names, so they are fixed per callsite by construction —
exactly the low-cardinality property the schema story needs. Values are
formatted to strings eagerly at construction. By default they are **interned**
(pooled) on the wire, since a span re-emits its fields on every poll and
low-cardinality values dedup well; a `~` prefix stores a value **inline**
instead, for high-cardinality values (request ids) that would otherwise grow the
string pool.

**The sync guard emits one enter/exit pair; the future combinator emits one
pair per poll.** The guard covers on-thread blocking work, so a single segment
is exact. `Instrumented::poll` enters at the top of each poll and exits when the
guard drops at the end of that poll, so a future that suspends and resumes shows
as multiple segments with gaps — matching what `tracing::Instrument` + the
tracing layer produce, and carrying per-segment worker attribution.

**`Entered` is `!Send` and must not be held across `.await`.** Same rule as
`tracing::span::Entered`, enforced the same way: a `PhantomData<*const ()>`
field makes the guard `!Send`, so holding one across an await in a `Send` task
is a compile error. The exit must land on the entering thread's stream ordered
after the enter; holding across an await would attribute other tasks' work to
the span. The async story is `instrument`, full stop. (The future combinator is
unaffected: its guard is a synchronous local inside `poll`.)

**`SpanCloseEvent` is emitted on drop of the `Dial9Span`,** whether the future
completed or was cancelled. Close is what lets the viewer finalize and recycle
the span id; a cancelled request still renders as a span ending at cancellation
time. `Dial9Span` is deliberately **not** `Clone` — one identity, one close.

**Off-runtime threads are silently skipped**, identical to the tracing layer:
every emit checks `Dial9Handle::is_enabled()` (and close uses
`Dial9Handle::try_current`, a no-op when no handle is installed). Wrappers are
always safe to leave in code paths that sometimes run outside a dial9-traced
runtime, and are no-ops (branch + return) when dial9 is disabled.

## Span ID space

The tracing layer uses `tracing`'s subscriber-allocated `span::Id` (small
integers starting at 1, recycled on close). Ad-hoc spans allocate from a
process-global `AtomicU64` counter **with the top bit set**
(`id = (1 << 63) | next`, `next` starting at 1), so the two producers can never
collide when both run in one process. The viewer keys spans by the id's string
form and already handles id recycling via `SpanClose`, so no viewer change is
needed. 2^63 ids does not wrap in practice (at 1B spans/sec: ~292 years).

## Tower layer semantics (v1)

- One span per `call`, created by the `make_span` closure (`named` for a fixed
  name, `new` for a per-request span). The span is attached to the inner
  service's response future via `instrument`.
- The response future is entered/exited per poll and closed when it resolves or
  is dropped (e.g. on cancellation). **Streaming bodies are not covered by the
  span** in v1: the span closes when the response future resolves (the response
  head for HTTP). Holding the span open until end-of-stream is a documented
  follow-up (requires an `http-body` dep and per-frame policy decisions).
- The layer is fully generic over the request/response types; nothing in it
  depends on `http`. The example program shows it on an axum stack because that
  is the motivating case.

## Feature flags & semver

- Core `span` module (macro, guard, future combinator): unconditional, no new
  deps. All new API, purely additive.
- `tower` feature: gates `Dial9SpanLayer`/`Dial9SpanService` (+ `tower-layer` /
  `tower-service` deps). Mirrored as a passthrough `tower` feature on the `dial9`
  umbrella crate.
- `#[traceevent(name = <expr>)]`: additive attribute on the existing derive; no
  effect on types that don't use it.
- No trace-format changes: only new instances of the existing self-describing
  span schemas. Old JS viewers render new traces unchanged.

## Open questions for review

1. Field value encoding: values are stringified (interned or inline) today,
   matching the tracing layer. A typed `Value` (varint for ints, etc.) would
   help future numeric aggregation in the viewer and remains a compatible schema
   addition later.
2. Tower `make_span` takes no arguments today (`Fn() -> Dial9Span`), so fields
   are constants or captured from the environment, not read off the request. A
   `Fn(&Req) -> Dial9Span` form would let callers pull request fields in, at the
   cost of tying the closure to the request type. Deferred until there is a
   concrete need.
3. Module name: `span` (shipped) vs `spans` vs re-exports at crate root.
4. Does the tower layer belong in this crate behind a feature (shipped), or in a
   separate `dial9-tower` crate? Feature keeps versioning simple; a separate
   crate keeps the core dep-free even at compile time.
