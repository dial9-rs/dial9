# Ad-Hoc Span Wrappers Design

Issue: [dial9-rs/dial9#498](https://github.com/dial9-rs/dial9/issues/498)

Status: **implemented.** The code lives in its own lightweight **`dial9-util`**
crate — `dial9-util/src/span/{mod,future,wire}.rs` and the tower layer in
`span/tower.rs` (feature `tower`) — re-exported from the `dial9` umbrella crate.
`dial9-util` depends only on `dial9-core` + `dial9-trace-format` (plus an
*optional* `tokio` for the worker index), so **spans work with or without
Tokio**. Tests are in `dial9-util/tests/span_wrappers.rs`, a runnable example is
`dial9/examples/adhoc_spans.rs`, and a head-to-head benchmark is
`dial9-util/benches/span_encode_bench.rs`. This document is the design rationale;
the code is the source of truth for the exact signatures.

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
appear in the viewer — and it should cost no more than hand-writing the trace
events yourself (see *Zero-cost*).

## Non-goals

- Replacing `Dial9TracingLayer`. Applications already on `tracing` should keep
  using it; the two coexist in one process (see *Span ID space*).
- Context propagation / implicit parenting. Contextual parents are unreliable
  across tasks (see the comment in `tracing_layer.rs::on_enter`); the viewer
  infers nesting from timestamp containment. We only support *explicit*
  parenting, same as the tracing layer.
- Late / deferred fields. Fields are captured when the span is built (that is
  what lets each callsite bake in a typed, compile-time schema). A value only
  knowable later (e.g. an HTTP status) is recorded by building the span where
  that value is in scope, not by mutating a live span.
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
| enter | `SpanEnter:<file>:<line>:<col>` (macro) or `SpanEnter:adhoc::runtime` (name-only) | base + user fields (**typed**) |
| exit  | `SpanExit:<file>:<line>:<col>` (macro) or `SpanExit:adhoc::runtime` (name-only) | base (minus `parent_span_id`) + `active_ns`, `idle_ns`, `poll_count`, `completed` + user fields |
| close | `SpanCloseEvent` (existing struct, private copy per producer) | `timestamp_ns`, `span_id` |

The schema id is the callsite (`file:line:col`), not the span name. The name may
be dynamic, so it rides in the `span_name` field (what the viewer reads), never
in the schema id. Keying the id on the callsite keeps schema cardinality bounded
by code, not data. User fields keep their Rust type on the wire — a `u64` is a
`Varint`, not a stringified pooled value.

No viewer changes are required. Old viewers render new traces; new viewers
render old traces. This satisfies the issue's "standardized format that can be
automatically rendered as spans in the UI": the format already exists, this
design just gives it a second producer.

## Zero-cost: `dial9_span!` compiles to hand-written cost

The design goal is that `dial9_span!` be a zero-cost abstraction over
hand-writing `#[derive(TraceEvent)]` structs and emitting them yourself. It is:
`span_encode_bench` puts the two head to head (construct id, emit enter → exit →
close, two typed `u64` fields), and they land within measurement noise
(~325 ns vs ~323 ns for the macro vs the hand-written span). What makes that
possible:

- **Typed, monomorphized events.** `dial9_span!(...)` expands, at each call site,
  to a pair of generated `#[derive(TraceEvent)]` structs (`SpanEnter:<file>:<line>:<col>`
  and the matching `SpanExit`) whose user fields keep their concrete Rust types.
  Each field's identifier doubles as its own type parameter (fields and type
  params are separate namespaces), so `order_id = 7u64` stores a `u64` and rides
  the wire as a `Varint` — no `format!`, no string, no `Vec`. Emitting is a
  direct `enc.encode(&Enter { … })`, monomorphized and inlinable — no boxed
  closure or function-pointer indirection.
- **No runtime schema map, lock-free registration.** The generated types carry
  their schema (name + typed `field_defs`) at compile time. Registration on the
  wire goes through the encoder's normal per-type cache — a lock-free
  `HashMap<TypeId, wire_id>` on the **per-thread** encoder, the same path every
  `#[derive(TraceEvent)]` event already takes. There is no span-owned schema map,
  and in particular no `Mutex` — unlike the tracing layer, which memoizes a
  runtime-built `Schema` in a shared `Mutex<HashMap<callsite, …>>` and locks it
  on every enter/exit. (The generated types stay on this dynamic per-`TypeId`
  path rather than claiming a `wire_slot` fast-path id: those static slots are a
  scarce resource — `STATIC_WIRE_ID_LIMIT` of them — and per-callsite span types
  could exhaust them.)
- **The literal name is a compile-time constant**, interned from a `const &str`
  each emit (a pool lookup, same as hand-writing) rather than stored.
- **One handle acquisition per enter/exit pair.** `enter()` fetches
  `Dial9Handle::current()` once and the `!Send` guard reuses it for the matching
  exit (safe: exit runs on the entering thread). The close event, on span drop,
  fetches its own handle since a future's drop may land on another thread.

The only allocation is a per-emit clone of an owned `String` field — and a
correct hand-written re-emitting span pays exactly that (interned ids are not
stable across flush cycles, so a stored value must be re-materialized each
emit). `Copy`/numeric fields are allocation-free.

### Enabling changes to the derive / trace-format

Three small, general, additive changes outside the `span` module let the derive
work for the generated types:

1. `#[traceevent(name = <expr>)]` on the `TraceEvent` derive: overrides the
   default event name (the struct name) with any `&'static str` expression, so a
   generated type can name itself `concat!("SpanEnter:", file!(), ":", line!(),
   ":", column!())`. Evaluated at the derive site, so the builtins resolve to the
   user's callsite.
2. Generics propagation in the derive (`split_for_impl`), so `#[derive(TraceEvent)]`
   works on the generic generated event structs. The generated impl also carries
   `#[allow(non_camel_case_types, non_snake_case)]` because the type params are
   named after user field idents.
3. `impl TraceField for &str` in `dial9-trace-format`, so a bare string-literal
   field (`service = "checkout"`, a `&'static str`) is zero-cost (stored as a
   `&'static str`, no `String` allocation).

## API surface

Module `dial9_util::span`, re-exported as `dial9::span`; the `dial9_span!` macro
is exported at the `dial9-util` crate root (and re-exported as `dial9::dial9_span`).
The core (macro + guard + future combinator) depends only on `dial9-core` +
`dial9-trace-format`. The tower layer sits behind a `tower` feature (adds the
small `tower-service` / `tower-layer` trait crates, not `tower` itself).

```rust
// Build a span, capturing the callsite and typed fields. tracing-style syntax:
//   bare  → keeps the Rust type (u64 → Varint); must be `TraceField + 'static`
//   %expr → Display-format to an owned String
//   ?expr → Debug-format to an owned String
// Returns an opaque per-callsite type implementing `Span`.
dial9_span!("db.load", order_id = id, retries = n, path = %p, cfg = ?c) -> impl Span;

pub trait Span: Sized {
    fn id(&self) -> u64;                                 // for explicit parenting
    fn with_parent_id(self, parent_span_id: u64) -> Self;
    fn with_parent(self, parent: &impl Span) -> Self;
    fn enter(&self) -> Entered<'_, Self>;               // sync RAII guard
    // + hidden __set_parent / __emit_enter / __emit_exit (macro-generated)
}

pub struct Dial9Span;                         // name-only span, runtime name
impl Dial9Span { pub fn new(name: impl Into<String>) -> Dial9Span; }
impl Span for Dial9Span { … }

pub struct Entered<'a, S: Span>;              // !Send; holds the enter handle;
                                             // emits exit on drop
pub trait Instrument: Future + Sized {
    fn instrument<S: Span>(self, span: S) -> Instrumented<Self, S>;
}
pub struct Instrumented<F, S: Span>;          // Future, transparent output
```

The span name must be a `&'static str` expression (it is baked in as a `const`);
for a runtime-chosen name use `Dial9Span::new`, which carries no user fields and
shares one `adhoc::runtime` schema. Span types are **not** `Clone` — one
identity, one close.

Tower layer (feature `tower`):

```rust
Dial9SpanLayer::new(|req: &Req| dial9_span!("rpc", route = %req.path())) // per-request
Dial9SpanLayer::new(|_req: &Req| dial9_span!("http_request"))            // fixed name
```

### Design decisions

**Field names are macro identifiers; values keep their type.** Names become
wire-schema field names, so they are fixed per callsite by construction — the
low-cardinality property the schema story needs. A bare value keeps its Rust
type (so it must be `TraceField + Clone + 'static`); `%`/`?` render any
`Display`/`Debug` value to an owned `String`. This replaces the earlier
stringify-everything approach and is what makes numeric fields zero-cost.

**The future combinator emits one enter + one completion exit, not a pair per
poll.** `Instrumented` emits `SpanEnter` on the first poll, then times each poll
and accumulates `active_ns` (summed poll durations) and `poll_count`; when the
future resolves (or is dropped/cancelled) it emits a single `SpanExit` carrying
`active_ns`, `idle_ns` (wall since enter minus active), `poll_count`, and a
`completed` flag. Per-poll segment detail is recoverable by correlating the
span's `[enter, exit]` window with the task's existing `PollStart`/`PollEnd`
events, so the span itself doesn't re-emit it — cheaper on the wire and the
completion event directly answers the analytics question (time distribution).
The **sync guard** emits one enter/exit pair for its single on-CPU segment,
reporting `(active = duration, idle = 0, poll_count = 1, completed = true)` — the
same schema, filled trivially.

Because a span type has a single `SpanExit` schema per callsite (used whether the
span is `enter`ed or `instrument`ed), the aggregate fields live on that one exit
event; the sync and async paths just populate them differently.

Viewer note: the viewer today derives a span's active time by pairing
enter/exit intervals, so it reads the single `enter → completion` interval as
active for the whole wall time. The accurate `active_ns`/`idle_ns` are present as
fields on the exit for analytics; teaching the viewer to prefer the reported
`active_ns` over interval-pairing for these spans is a cosmetic follow-up, not a
blocker (nothing breaks — the span renders and the data is present).

**`Entered` is `!Send` and must not be held across `.await`.** Same rule as
`tracing::span::Entered`, enforced the same way: a `PhantomData<*const ()>`
field makes the guard `!Send`, so holding one across an await in a `Send` task
is a compile error. The exit must land on the entering thread ordered after the
enter; holding across an await would attribute other tasks' work to the span.
Being `!Send` is also what lets the guard safely cache the enter-time handle for
the exit. The async story is `instrument`, full stop.

**`SpanCloseEvent` is emitted on drop of the span,** whether the future completed
or was cancelled. Close is what lets the viewer finalize and recycle the span id;
a cancelled request still renders as a span ending at cancellation time. For the
future combinator, `Instrumented` owns the span, so dropping the future drops the
span and fires close.

**Off-runtime threads are silently skipped**, identical to the tracing layer:
enter/exit go through `Dial9Handle::with_encoder` (a no-op when disabled) and
close checks `Dial9Handle::is_enabled()`. Wrappers are always safe to leave in
code paths that sometimes run outside a dial9-traced runtime, and are no-ops
(branch + return) when dial9 is disabled.

## Span ID space

The tracing layer uses `tracing`'s subscriber-allocated `span::Id` (small
integers starting at 1, recycled on close). Ad-hoc spans allocate from a
process-global `AtomicU64` counter **with the top bit set**
(`id = (1 << 63) | next`, `next` starting at 1), so the two producers can never
collide when both run in one process. The viewer keys spans by the id's string
form and already handles id recycling via `SpanClose`, so no viewer change is
needed. 2^63 ids does not wrap in practice (at 1B spans/sec: ~292 years).

## Tower layer semantics (v1)

- One span per `call`, from a single `make_span` closure (`Fn(&Req) -> impl Span`)
  passed to `new`. It receives the request, so it can attach request-derived
  fields (route, method, …), or ignore it (`|_req|`) for a fixed name. The span
  is attached to the inner service's response future via `instrument`, so the
  layer is generic over the span type the closure returns.
- The response future emits one enter + one completion exit (with the
  active/idle/poll aggregate) and closes when it resolves or is dropped (e.g. on
  cancellation). **Streaming bodies are not covered by the span** in v1: the span
  closes when the response future resolves (the response head for HTTP). Holding
  the span open until end-of-stream is a documented follow-up (requires an
  `http-body` dep and per-frame policy decisions).
- The layer is fully generic over the request/response types; nothing in it
  depends on `http`. The example program shows it on an axum stack because that
  is the motivating case.

## Crate placement & Tokio-independence

The span code is its own crate, **`dial9-util`**, not part of `dial9-tokio-telemetry`,
because spans should work even in programs that don't use Tokio. The emit path
needs only `dial9-core` (the `Dial9Handle`/encoder/clock) and `dial9-trace-format`
— nothing Tokio-specific — so a plain-threads program that installs the core
recorder can emit spans too.

The one Tokio touch-point is the `worker_id` wire field. `dial9-util` reads it
from `tokio::runtime::worker_index()` behind an **optional `tokio` feature**;
off a runtime, or with the feature off, `worker_id` is a sentinel (unknown).
That keeps the crate Tokio-free by default while still stamping the worker index
when Tokio is in use. (`worker_index()` is a `tokio_unstable` API; the workspace
builds with `--cfg tokio_unstable`.) This is a deliberately simpler source than
the tracing layer's global worker id — `worker_id` is nice-to-have metadata, and
per-poll worker attribution is recoverable from the task's poll events anyway.

`dial9-util` depends on `dial9-core`, **not** on `dial9-tokio-telemetry`, so there
is no cycle with `dial9-tokio-telemetry`'s (optional) dependency on `dial9-utils`.
The span tests need a traced Tokio runtime, so they pull `dial9-tokio-telemetry`
as a **dev-dependency** only — allowed, since it never forms a normal-dependency
cycle. `SpanCloseEvent` is a tiny `wire_slot` event; the tracing layer and
`dial9-util` each keep a private copy (identical schema, distinct producers).

## Feature flags & semver

- `dial9-util` core (macro, guard, future combinator): depends only on
  `dial9-core` + `dial9-trace-format`. All new API, purely additive.
- `tokio` feature (`dial9-util`): opts into reading the Tokio worker index for
  `worker_id`; without it spans still work (worker unknown). Enabled by the
  `dial9` umbrella's `tokio` feature.
- `tower` feature: gates `Dial9SpanLayer`/`Dial9SpanService` (+ `tower-layer` /
  `tower-service` deps). Mirrored as a passthrough `tower` feature on `dial9`.
- Derive / trace-format changes (`name = <expr>`, generics propagation,
  `TraceField for &str`) are additive and have no effect on existing events.
- No trace-format *wire* changes: only new instances of the existing
  self-describing span schemas. Old JS viewers render new traces unchanged.

## Open questions for review

1. Module name: `span` (shipped) vs `spans` vs re-exports at crate root.
2. Does the tower layer belong in this crate behind a feature (shipped), or in a
   separate `dial9-tower` crate? Feature keeps versioning simple; a separate
   crate keeps the core dep-free even at compile time.
