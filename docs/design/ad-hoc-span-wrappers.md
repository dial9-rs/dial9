# Ad-Hoc Span Wrappers Design

Issue: [dial9-rs/dial9#498](https://github.com/dial9-rs/dial9/issues/498)

Status: **implemented.** The code lives in the **`dial9-utils`** crate
(dial9's runtime-integration utilities) — `dial9-utils/src/span/{mod,future,wire}.rs`
and the tower layer in `dial9-utils/src/tower.rs` (feature `tower`). Used as
`dial9_utils::span` — deliberately *not* re-exported from the `dial9` umbrella,
so the umbrella doesn't pull `dial9-utils`'s S3/AWS deps (see *Crate placement*).
It's built on `dial9-core` + `dial9-trace-format`, so the
span emit needs **no Tokio runtime** (the optional task-id field is absent
outside a Tokio task). Tests are in `dial9-utils/tests/span_wrappers.rs`, a
runnable example is `dial9-utils/examples/adhoc_spans.rs`, and a head-to-head
benchmark is `dial9-utils/benches/span_encode_bench.rs`. This document is the design
rationale; the code is the source of truth for the exact signatures.

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
- Log-style events (one-shot points without duration). `record_event` with a
  custom `#[derive(TraceEvent)]` struct already covers that.

> **Late / deferred fields are now supported** (they were originally a
> non-goal). A value only knowable after the span is built — an HTTP status, a
> response size — is declared as `name: Type` and set through a returned `slots`
> handle; it rides the completion event. See *[Late fields](#late-fields)*.

## Wire format: reuse the existing standardized span events

The viewer already recognizes span events *by schema-name prefix*
(`trace_analysis.js::buildSpanData`): `SpanEnter:*`, `SpanExit:*`, and
`SpanCloseEvent`, with base fields `dial9.tokio.task_id`, `span_id`,
`parent_span_id`, `span_name` and arbitrary extra fields. The ad-hoc wrappers
emit exactly this format:

| event | schema name | fields |
|---|---|---|
| enter | `SpanEnter:dial9_utils::adhoc_<col>:<file>:<line>` (macro) or `SpanEnter:dial9_utils::runtime:runtime:0` (name-only) | base + user fields (**typed**) |
| exit  | `SpanExit:dial9_utils::adhoc_<col>:<file>:<line>` (macro) or `SpanExit:dial9_utils::runtime:runtime:0` (name-only) | base (minus `parent_span_id`) + `active_ns`, `idle_ns`, `poll_count`, `completed` + user fields |
| close | `SpanCloseEvent` (existing struct, private copy per producer) | `timestamp_ns`, `span_id` |

The macro schema id contains the callsite (`file:line:col`), not the span name,
and follows the viewer's `target::name:file:line` parser. The runtime name rides
in the `span_name` field (what the viewer reads), never in the schema id. Keying
the id on the callsite keeps schema cardinality bounded by code, not data. User
fields keep their Rust type on the wire — a `u64` is a `Varint`, not a
stringified pooled value.

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

- **Typed, concrete events.** `dial9_span!(...)` expands, at each call site,
  to a pair of generated `#[derive(TraceEvent)]` structs with matching,
  callsite-derived `SpanEnter`/`SpanExit` names whose user fields keep their
  concrete Rust types.
  Each eager field carries its declared type (`order_id: u64 = 7u64`), so the
  structs are concrete — no generic type parameters — and `order_id` stores a
  `u64` and rides the wire as a `Varint`: no `format!`, no string, no `Vec`.
  Emitting is a direct `enc.encode(&Enter { … })`, inlinable — the library-owned
  span carrier is generic over zero-sized call-site emitter closures, with no
  boxing or function-pointer indirection.
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
- **The span name retains its input representation.** A string literal is stored
  as an `&str` with no allocation; an owned runtime `String` is also accepted
  without another conversion or allocation. Both are interned when emitted.
- **One handle acquisition per enter/exit pair.** `enter()` fetches
  `Dial9Handle::current()` once and the `!Send` guard reuses it for the matching
  exit (safe: exit runs on the entering thread). The close event, on span drop,
  fetches its own handle since a future's drop may land on another thread.

The only allocation is a per-emit clone of an owned `String` field — and a
correct hand-written re-emitting span pays exactly that (interned ids are not
stable across flush cycles, so a stored value must be re-materialized each
emit). `Copy`/numeric fields are allocation-free.

### Enabling changes to the derive / trace-format

Three small, general, additive capabilities outside the `span` module let the
derive work for the generated types:

1. `#[traceevent(name = <expr>)]` on the `TraceEvent` derive: overrides the
   default event name (the struct name) with any `&'static str` expression, so a
   generated type can name itself `concat!("SpanEnter:", file!(), ":", line!(),
   ":", column!())`. Evaluated at the derive site, so the builtins resolve to the
   user's callsite.
2. `#[traceevent(name = "...")]` on a field: gives a Rust field a canonical wire
   name such as `dial9.tokio.task_id`, while encoding still reads the original
   Rust identifier.
3. `impl TraceField for &str` in `dial9-trace-format`, so a string-literal field
   (`service: &'static str = "checkout"`) is zero-cost (stored as a
   `&'static str`, no `String` allocation).

The generated event structs are concrete (each eager field declares its type),
so the derive needs no generic-parameter support for this feature.

## API surface

Module `dial9_utils::span`; the `dial9_span!` macro is exported at the
`dial9-utils` crate root. (Not re-exported from the `dial9` umbrella — that would
force `dial9-utils`'s S3/AWS deps onto every `dial9` build; users depending on
spans take `dial9-utils` directly.)
The core (macro + guard + future combinator) depends only on `dial9-core` +
`dial9-trace-format`. The tower layer sits behind a `tower` feature (adds the
small `tower-service` / `tower-layer` trait crates, not `tower` itself).

```rust
// Build a span, capturing the callsite and typed fields. Field syntax:
//   key: Type = value → keeps the Rust type (u64 → Varint); Type must be `TraceField`
//   key = %expr       → Display-format to an owned String
//   key = ?expr       → Debug-format to an owned String
// Returns an opaque per-callsite type implementing the sealed `Span` trait.
dial9_span!("db.load", order_id: u64 = id, retries: u32 = n, path = %p, cfg = ?c) -> impl Span;

pub trait Span: private::SpanImpl + Sized {              // sealed
    fn id(&self) -> SpanId;                              // for explicit parenting
    fn with_parent_id(self, parent_span_id: SpanId) -> Self;
    fn with_parent(self, parent: &impl Span) -> Self;
    fn enter(&self) -> Entered<'_, Self>;               // sync RAII guard
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

The macro accepts any span name implementing `AsRef<str>`, including string
literals, borrowed strings, and owned runtime `String`s. `Dial9Span::new` is a
name-only alternative sharing one runtime schema. `Span` is sealed and span
types are **not** `Clone`: one identity, one close.

Tower layer (feature `tower`):

```rust
Dial9SpanLayer::new(|req: &Req| dial9_span!("rpc", route = %req.path())) // per-request
Dial9SpanLayer::new(|_req: &Req| dial9_span!("http_request"))            // fixed name

// Response-derived (late) fields: make_span returns (span, finish); finish
// captures the slots and is called with the successful response.
Dial9SpanLayerWithResponse::new(|req: &Req| {
    let (span, slots) = dial9_span!("http_request", route = %req.path(), status: u16);
    (span, move |resp: &Resp| slots.status.set(resp.status()))
})
```

### Design decisions

**Field names are macro identifiers; values keep their type.** Names become
wire-schema field names, so they are fixed per callsite by construction — the
low-cardinality property the schema story needs. An eager field is written
`key: Type = value` and keeps its Rust type (the type must be `TraceField +
Clone`); `%`/`?` render any `Display`/`Debug` value to an owned `String`. The
required type is what keeps the generated event struct concrete (no generics)
while still zero-cost — numeric fields ride the wire as their native type.

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

## Late fields

A field written as `name: Type` (a type instead of `= value`) is *late*: its
value is not known at the call site but set later, before the span completes.
Declaring at least one late field changes the macro's return from a bare span
to `(span, slots)`:

```rust
let (span, slots) = dial9_span!("request", route: &'static str = "/checkout", status: u16, bytes: u64);
async move {
    let resp = handle().await;
    slots.status.set(resp.status);   // recorded on completion
    slots.bytes.set(resp.len());
}
.instrument(span)
.await;
```

`slots.<name>` is a `Slot<Type>`, a write-once handle; `set` takes the first
write and ignores the rest (it wraps a `OnceLock`). A span with only eager
fields is returned bare exactly as before — **no source change for existing
callers.** The type must satisfy `Option<Type>: TraceField` (the numeric types,
`bool`, `String`); render anything else to a `String` before `set`.

**Why this shape, and why not the `tracing` one.** The `tracing` recipe is
`field::Empty` + `span.record(..)` later. It relies on two things dial9 ad-hoc
spans don't have: a live, subscriber-owned mutable field bag (a dial9 span *is*
the producer — it emits its own events and has nothing to mutate after the
fact), and untyped fields (dial9 fields are typed and monomorphized per
callsite, so a late field's type must be named up front). Two consequences fall
out directly:

- A late value can land only on the **completion (exit) event**, never enter —
  which composes with the decision above that the completion event is where
  post-hoc metadata already lives. Late fields are just more of it.
- The setter and the span both need a handle to the same storage, because the
  span is moved into `.instrument(..)` while `set` is called from inside the
  body. That shared storage is an `Arc<OnceLock<T>>`: one clone in the span, one
  in the `Slot`. On exit the span reads `get().cloned()` → `Some(v)` or `None`.

**Mechanics.** The macro's muncher sorts fields into an eager `(key, value)`
list and a late `(key, type)` list and dispatches on whether the late list is
empty: empty → the eager build; non-empty → `__dial9_span_build_late!`, which
adds one `Option<Type>` column per late field to the `SpanExit` struct, one
`Arc<OnceLock<Type>>` per late field to the carrier payload, and a generated
`__Dial9Slots` handle. `SpanEnter` is untouched.

**Backwards compatibility.** Late fields only *add* columns to a callsite's
`SpanExit` schema — always safe per the trace-format rules (the schema is on
the wire ahead of the events; old traces just lack the columns). The stored
type is `Option<Type>`, whose wire tag carries a present/absent discriminant,
so a never-set field is a genuine `None`, not a zero sentinel. No decoder
change is needed to read these traces; surfacing the new columns in the viewer
detail panel is folded into the same cosmetic follow-up as `active_ns` above.

**Cost.** Opt-in and localized. An eager-only callsite is byte-for-byte the old
path — no `Arc`, no atomics, still allocation-free (the zero-cost benchmark is
unaffected). A late field costs one shared allocation plus an atomic write/read
— the irreducible cost of moving a value across the `.instrument(..)` boundary.

**Through the tower layer.** The response-derived case (a status code, a body
size) is exactly the per-request shape the tower layer serves, so it has
first-class support via `Dial9SpanLayerWithResponse` (see *Tower layer
semantics*). The obstacle is that the slots type is generated per call site and
cannot be named — so a separate `on_response(|slots, resp| …)` closure could
neither name nor infer its argument. The resolution is to have `make_span`
return `(span, finish)` where `finish` is a closure that **captures the slots
itself** and receives only the response; the un-nameable type never appears in
a signature the user writes. `finish` runs the instant the inner future
resolves (before the span's completion event), so the values it sets ride that
event.

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
- **Response-derived (late) fields** use a second layer,
  `Dial9SpanLayerWithResponse`, whose `make_span` returns `(span, finish)`.
  `finish: Fn(&Resp)` is called with the successful response before the span
  closes — the place to set late `Slot`s captured from `make_span`'s
  `dial9_span!` (see *Late fields*). Internally it wraps the inner future in an
  `OnResponse` combinator that runs `finish` on `Poll::Ready(Ok(_))`, nested
  inside the usual `Instrumented`, so the set values are present when the
  completion event is emitted. It is a distinct type (not a method on
  `Dial9SpanLayer`) because the eager `make_span` returns `impl Span` while the
  late one returns a tuple — two incompatible return shapes that would collide
  under one `Service` impl. `finish` is **not** called on the error path in v1.

## Crate placement & Tokio-independence

The span code lives in **`dial9-utils`** (dial9's runtime-integration utilities
crate), *not* in `dial9-tokio-telemetry`, because spans should work even in
programs that don't use the Tokio runtime integration. The emit path needs only
`dial9-core` (`Dial9Handle`/encoder/clock) and `dial9-trace-format` — nothing
that requires a Tokio *runtime* — so a plain-threads program that installs the
core recorder can emit spans too.

The one Tokio touch-point is the optional `dial9.tokio.task_id` wire field. It
comes from `tokio::task::try_id()` and is absent outside a Tokio task. The value
matches dial9's poll events, so the viewer can reconstruct the span's active
intervals even when the task migrates between workers. The field also carries
the `dial9.role = tokio.task_id` schema annotation.

`dial9-utils` depends on `dial9-core`, **not** on `dial9-tokio-telemetry` — while
`dial9-tokio-telemetry` optionally depends on `dial9-utils` (its S3 uploader) — so
there is no cycle. The span tests need a traced Tokio runtime, so they pull
`dial9-tokio-telemetry` as a **dev-dependency** only, which never forms a
normal-dependency cycle. `SpanCloseEvent` is a tiny `wire_slot` event; the tracing
layer and `dial9-utils` each keep a private copy (identical schema, distinct
producers).

Trade-off: `dial9-utils` also carries the S3 uploader (`aws-sdk-s3`), so
depending on it *just* for spans is heavier than a dedicated crate would be —
which is why the `dial9` umbrella does **not** re-export spans (that would force
AWS onto every `dial9` build). Spans users take `dial9-utils` directly. A lighter
home (feature-gating S3, or a dedicated crate) is a possible follow-up.

## Feature flags & semver

- `dial9-utils` span module (macro, guard, future combinator): built on
  `dial9-core` + `dial9-trace-format`. All new API, purely additive.
- The optional task id uses `tokio::task::try_id()`; it is absent outside a
  Tokio task, so no Tokio runtime is required to emit spans.
- `tower` feature (`dial9-utils`): gates `Dial9SpanLayer`/`Dial9SpanService`
  (+ `tower-layer` dep; `tower-service` was already present). Mirrored as a
  passthrough `tower` feature on `dial9`.
- Derive / trace-format changes (`name = <expr>`, `TraceField for &str`) are
  additive and have no effect on existing events.
- No trace-format *wire* changes: only new instances of the existing
  self-describing span schemas. Old JS viewers render new traces unchanged.

## Open questions for review

1. Module name: `span` (shipped) vs `spans` vs re-exports at crate root.
2. Does the tower layer belong in this crate behind a feature (shipped), or in a
   separate `dial9-tower` crate? Feature keeps versioning simple; a separate
   crate keeps the core dep-free even at compile time.
