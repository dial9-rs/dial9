# Span wire contract — the spec for spans

> **Status:** normative. This document specifies the rules dial9 uses to *read*
> spans from a trace. It is the contract a producer must satisfy so that spans
> decode correctly, independent of *how* they were emitted (the built-in
> `Dial9TracingLayer`, a hand-written `#[derive(TraceEvent)]` struct, or a future
> bridge from another instrumentation system).
>
> Companion to [`generalized-span-flamegraphs.md`](generalized-span-flamegraphs.md),
> which motivates the feature and defines the folded Parquet schema. This
> document is narrower: it pins down the on-wire → decoded-span mapping so a
> producer other than the tracing layer can conform.

## Audience and scope

Read this if you are:

- writing a producer that emits spans **without** the tracing layer (custom
  `TraceEvent` structs, an OTel/other bridge, a manual instrument);
- modifying dial9's span decoder and need the invariants it must preserve;
- reasoning about why a span decoded the way it did.

The decoder has two independent implementations that this contract binds:

1. **The folder** (Rust, `dial9-viewer/src/ingest/decode/`) — produces the
   authoritative `spans/` Parquet rows (`ResolvedSpan`) and enriches samples.
2. **The viewer** (JS, `dial9-viewer/ui/trace_analysis.js::buildSpanData`) —
   reconstructs raw span timelines for the interactive trace view.

Where the two diverge it is called out explicitly. A conforming producer must
satisfy both.

---

## 1. The span model dial9 reads

dial9 does not read a span as a single record. It reads **three event kinds**
and reconstructs a span from them:

| Event kind | Emitted | Purpose to the decoder |
|---|---|---|
| **Span enter** | Every time the span becomes on-CPU (guard taken / poll begins inside it) | Marks the start of one *entered execution interval*. |
| **Span exit** | Every time the span leaves CPU (guard released / poll yields) | Closes the most recent entered interval. |
| **Span close** | Once, at lifecycle end (the span is dropped/finished) | The authoritative summary row: identity, lifecycle bounds, final attributes, quality. |

A span therefore has:

- **one lifecycle**: `[start_ns, close_ns)` — carried by the close event;
- **zero or more entered intervals**: `[enter_ts, exit_ts)` pairs — carried by
  enter/exit events. Every interval is **half-open**.

This split is deliberate. An async span holds its guard across `.await` points,
so it enters and exits once per poll and may migrate worker threads between
polls. The lifecycle spans the whole operation; the entered intervals are only
the slices during which it was actually on CPU. Only the close event is required
for a span to produce a row; enter/exit events add local execution detail.

> **Key consequence for producers:** dial9 attributes CPU/scheduler samples
> **only to entered intervals, never to the lifecycle envelope**
> (`decode/attribution.rs`). A span that is exited (waiting) must not claim
> samples that fire during its idle gap. If you emit only a close event with no
> enter/exit pairs, the span appears with correct elapsed time but no on-CPU
> detail — all elapsed time is reported as `unknown_ns`.

---

## 2. Event recognition — how the decoder classifies an event

Events are classified **by their schema name prefix**, then by field content.
The folder's dispatch (`events.rs::decode_trace`) is the normative rule:

### 2.1 Name prefixes

| Prefix / exact name | Classified as |
|---|---|
| `SpanClose__…` or exact `SpanCloseEvent` | span close |
| `SpanEnter:…` or `SpanEnter__…` | span enter |
| `SpanExit:…` or `SpanExit__…` | span exit |

Two naming conventions exist because two producers exist:

- **Colon form** (`SpanEnter:{target}::{name}:{file}:{line}`) — the *dynamic*
  schema name the tracing layer builds per callsite
  (`tracing_layer.rs::build_callsite_schemas`).
- **Double-underscore form** (`SpanEnter__{Type}`) — a hand-written
  `#[derive(TraceEvent)]` struct's wire name is its Rust identifier, and a Rust
  identifier **cannot contain `:`**. So struct-derived producers use `__`
  instead. The suffix after `SpanEnter__` is a schema discriminator only; it does
  not affect classification.

The exact names `SpanCloseEvent` (folder) and `SpanEnterEvent`/`SpanExitEvent`
(JS viewer only) are accepted for the built-in producer's own struct names.

> **Rule P1 (naming):** A custom producer must name its structs
> `SpanEnter__{Type}`, `SpanExit__{Type}`, `SpanClose__{Type}`, using the **same
> `{Type}` suffix** for all three of one logical span type. `{Type}` must be a
> valid Rust identifier (no `:`; `__` is legal in identifiers).

### 2.2 Dialect selection — modern vs legacy

Within each event kind, the decoder picks a **dialect** by field content, not by
name:

- **Modern**: the event carries `span_instance_id > 0`. Decoded as
  `SpanEnterEvent` / `SpanExitEvent` / `SpanCloseSummary`.
- **Legacy**: `span_instance_id` is absent or `0`, but `span_id > 0`. Decoded as
  `LegacySpanEnterEvent` / `LegacySpanExitEvent` / `LegacySpanCloseEvent`.

The decision cascade for each event (from `decode_trace`):

```text
try modern struct
  ├─ span_instance_id > 0  → MODERN
  ├─ deserialized but id==0 → fall through to legacy (span_id > 0)
  └─ deserialize error       → try legacy struct (span_id > 0)
```

`span_instance_id == 0` is reserved as a sentinel ("telemetry was disabled at
span creation", or an old-producer event that only has `span_id`). A modern
producer **must never** emit `span_instance_id == 0` for a live span.

Modern and legacy spans may coexist in one file (e.g. a library using the old
format alongside a service on the new format); they are resolved independently.

> **New producers should always emit the modern dialect.** The legacy dialect is
> a read-compatibility path for traces from old producers; it is documented here
> (§5) only so the contract is complete.

---

## 3. Modern dialect — field contract

### 3.1 Span enter / span exit

The decoder reads only what it needs to pair intervals. From
`events.rs::SpanEnterEvent` / `SpanExitEvent`:

| Field | Type | Required by folder | Meaning |
|---|---|---|---|
| `timestamp_ns` | varint (monotonic ns) | **yes** | When the interval boundary occurred. |
| `span_instance_id` | varint | **yes** (`> 0`) | Stable instance identity; the pairing lane. |
| `tid` | varint | **yes** | OS thread id; part of the pairing key. |

The tracing layer *also* writes `worker_id`, `span_id`, `parent_span_id`, and
`span_name` plus user fields on enter/exit. Of these, `worker_id` is read
indirectly: it is the lane used to infer the span's owning task from the poll
timeline (§4.7). The rest exist for the JS viewer's raw-timeline reconstruction
and human inspection. A minimal folder-only producer may omit them, but emitting
`worker_id` (for task association) and `span_name` (for the viewer) is
recommended.

> **Rule P2 (pairing key):** enter/exit are paired by the composite key
> `(span_instance_id, tid)`. An enter and its matching exit **must carry the same
> `span_instance_id` and the same `tid`**. If execution moves to another thread,
> emit an exit on the old `tid` first and a fresh enter on the new `tid`. Do not
> emit an enter on one `tid` and its exit on another.

### 3.2 Span close (the authoritative summary)

From `events.rs::SpanCloseSummary`. This event alone must be sufficient to write
a useful `spans/` row even if **no enter/exit for the instance appears in the
same source file** (the span may have started in an earlier, un-folded file).

| Field | Type | Required | Meaning / rule |
|---|---|---|---|
| `timestamp_ns` | u64 mono | **yes** | Lifecycle **close** time. |
| `span_instance_id` | u64 | **yes** (`> 0`) | Stable instance identity. See §4.1. |
| `start_timestamp_ns` | u64 mono | **yes** | Lifecycle **start** (creation) time. `elapsed_ns = close − start`. |
| `first_enter_timestamp_ns` | `Option<u64>` | no | Absent if never entered. |
| `active_ns` | u64 | recommended | Producer-accumulated active **thread** time (sum of balanced enter→exit intervals). `0` = not reported; the folder then falls back to locally computed thread time. |
| `span_name` | String | **yes** | Display/runtime name. Participates in `span_type_uid`. |
| `target` | String | recommended | Module/target. Participates in `span_type_uid`. |
| `file` | `Option<String>` | recommended | Callsite file. Participates in `span_type_uid`. |
| `line` | `Option<u32>` | recommended | Callsite line. Participates in `span_type_uid`. |
| `parent_span_instance_id` | `Option<u64>` | no | Explicit stable parent **instance id** (not the wire id). `None` for a root — never fabricated. Folder computes `parent_span_uid` from it. See §4.6. |
| `attributes` | `Vec<(String,String)>` | no | Final close-time key/value attributes. This dedicated field is the **only** attribute channel the folder reads — see §6.5. |
| `unbalanced_enters` | u32 | no (default 0) | Producer's own count of unbalanced enter/exit at close. The decoder **independently** detects local imbalance from the actual events and adds this on top; report it only for imbalance the decoder cannot see (enters/exits in an earlier file). See §6.4. |

> **Removed flags.** Earlier drafts carried three more close flags —
> `concurrent`, `saturated`, and `loss_observable`. They are **not** part of this
> contract (§6.4 explains why). `concurrent` survives only as a *derived output*
> (the decoder infers it from thread-time vs. wall-time); a producer does not
> emit it.

All fields below `span_name` are `#[serde(default)]`: a producer that omits them
emits an event that decodes with zeros/empties. A clean producer emitting
balanced enter/exit pairs sets **no** quality flag at all.

The close struct in the tracing layer is marked `#[traceevent(wire_slot)]` (an
encoder fast-path opt-in); this is an encoding optimization, not part of the
read contract.

> **Rule P3 (close is authoritative):** emit exactly one close event per span
> instance, at lifecycle end, carrying at minimum `timestamp_ns`,
> `span_instance_id`, `start_timestamp_ns`, and `span_name`. `unbalanced_enters`
> defaults to 0; set it only when you know about cross-file imbalance (§6.4).
> Never inflate `active_ns`.

---

## 4. Derived semantics (what the decoder computes from your events)

These are the rules the folder applies in `span_builder.rs::SpanCandidate::finalize`
and `interval_pairing.rs`. A producer does not emit these values but must
understand them to emit events that decode sensibly.

### 4.1 Identity

```text
span_uid      = BLAKE3(boot_id ‖ span_instance_id_le)[0..16]
span_type_uid = BLAKE3(kind ‖ 0 ‖ target ‖ 0 ‖ name ‖ 0 ‖ file ‖ 0 ‖ line
                       [ ‖ "\0dial9:schema-name\0" ‖ len ‖ schema_name ])[0..16]
```

- **`span_instance_id`** must be **process-local, monotonically increasing, and
  never recycled**. The tracing layer allocates from a global `AtomicU64`
  starting at 1 (0 is the disabled sentinel; §2.2). `tracing::span::Id` values
  *are* recycled and must **not** be used as the instance id.
- **`boot_id`** namespaces instance ids across processes so `span_uid` is stable
  across a process's segments. The folder resolves it, not the producer:
  - `SegmentMetadata` entry `boot_id` present → `identity_quality = "metadata"`
    (authoritative);
  - else parsed from a namespaced source-key path
    `…/{date}/{HHMM}/{service}/{host}/{boot_id}/{file}` where `boot_id` matches
    `{4-alpha}-{digits}` → `identity_quality = "path"` (authoritative);
  - else → `identity_quality = "flat"` (cannot claim cross-file stability).
- **`span_type_uid`** groups instances of the same type. The runtime `name`
  always participates. For struct-derived (`SpanEnter__{Type}`) schemas the
  `{Type}` suffix is folded in as an extra discriminator so two struct types
  that share a runtime `name` stay distinct.

> **Rule P4 (identity):** allocate a nonzero, never-recycled `span_instance_id`
> per span instance. To get `identity_quality` better than `"flat"`, either
> write a `boot_id` entry into segment metadata, or write segments under the
> namespaced path layout. Keep `name`/`target`/`file`/`line` **stable across
> instances of the same type** so they group.

### 4.2 Interval pairing

`interval_pairing.rs::pair_intervals`:

1. All enter/exit events (across all keys) are sorted by
   `(timestamp_ns, decode_sequence)`. `decode_sequence` is the wire-decode order,
   assigned by the decoder — it breaks ties for events at the same timestamp.
2. Per key (`(instance_id, tid)` modern; `span_id` legacy), a **LIFO stack**:
   enter pushes, exit pops the most recent enter.
3. An exit matches only if `exit_ts >= enter_ts`; it produces the half-open
   interval `[enter_ts, exit_ts)`. A zero-duration interval (`enter_ts ==
   exit_ts`) is valid.
4. Unmatched exits (no enter on the stack) and unmatched enters (still on the
   stack at end) are counted and surface as `unbalanced_exits` /
   `unbalanced_enters`.

> **Consequence:** an exit encoded *before* an enter at the same timestamp stays
> unmatched. Emit enter before exit. Nesting/re-entry on the same key is fine and
> pairs LIFO.

### 4.3 Time accounting and the five-way invariant

For every resolved span:

```text
elapsed_ns = on_cpu_ns_est + blocked_ns_est + async_wait_ns
           + scheduler_delay_ns + unknown_ns
```

(nullable estimates contribute zero when null). The decoder **enforces** this: if
the classified categories overflow or exceed `elapsed_ns`, the whole
classification is discarded, `unknown_ns = elapsed_ns`, and attribution flag
bit 1 is set. dial9 never converts uncertainty into a plausible zero.

Derived time fields:

- `elapsed_ns` = close − start (wall clock, via `ClockSync` offset; raw mono if
  no offset).
- `observed_active_wall_ns` = union of entered intervals in this file.
- active **thread** time = sum of intervals (may exceed wall if concurrent).
- `active_ns` = producer `active_ns` if `> 0`, else local thread time, else
  `None`.
- `concurrent` (**derived output**) = thread time > wall time + 1. The producer
  does not emit this; the decoder infers it from the intervals it sees.

`details_complete` is `true` only when **all** hold: entered locally (has
intervals) · no unbalanced enters/exits · lifecycle start ≥ file boundary (first
`ClockSync` mono in the file) · `identity_quality ∈ {metadata, path}`. Legacy
spans are therefore never `details_complete`.

### 4.4 Attribution flags (bitfield)

| Bit | Meaning |
|---|---|
| 0 | profiler metadata missing |
| 1 | sample drops detected **or** attribution accounting invalid |
| 2 | worker/tid attribution ambiguous |
| 3 | wake classification unavailable |

### 4.5 Sample attribution

`attribution.rs` sweeps CPU/sched samples against **entered intervals only**
(mapped to wall clock), never the lifecycle envelope. A sample inside multiple
selected spans is counted once per span for inclusive per-span counts, but once
overall in a flamegraph. This is why balanced, tight enter/exit pairs matter:
they are the only thing that earns a span its on-CPU flamegraph.

### 4.6 Parentage

dial9 distinguishes **two relationships that must not be conflated**:

- **Lifecycle parent** (this section): a *recorded* child→parent pointer set at
  span creation. This is parentage.
- **Runtime enclosure** (§4.5): interval containment used for sample
  attribution, per-worker. A span that encloses another on CPU is **not**
  necessarily its parent, and a parent need not enclose its child on CPU.

**Only explicitly recorded parentage is represented.** The tracing layer
resolves the parent from `attrs.parent()` alone (`span!(parent: &x, …)`), never
the contextual current-span, because contextual parenting is unreliable across
async tasks sharing a worker thread (`tracing_layer.rs::on_enter`). dial9 does
**not** infer a lifecycle parent from timestamp containment or nesting. A missing
parent is *unknown* — never an asserted root.

**Two different parent fields carry two different ids:**

| Field | Event | Value | Consumer |
|---|---|---|---|
| `parent_span_instance_id: Option<u64>` | **close** | parent's **stable instance id** | the folder → `parent_span_uid` |
| `parent_span_id: Option<u64>` | **enter** | parent's **recycled tracing wire id** | JS viewer only (interactive nesting / depth) |

The durable link is computed by the folder:

```text
parent_span_uid = BLAKE3(boot_id ‖ parent_span_instance_id_le)[0..16]
```

This is a **content-addressed pointer**. The parent's row need **not** be in the
same source file; the link resolves whenever the parent's own close event
computes the same `span_uid` (same `boot_id` + instance id). A dangling pointer
(parent lives in an un-folded file) is expected and harmless — the folder stores
the pointer, it does not materialize a tree. Cross-file stability of the pointer
requires authoritative `identity_quality` (`metadata`/`path`) for **both** parent
and child (§4.1); under `flat` identity the uid is still computed but is not
cross-file stable.

The enter event's `parent_span_id` is a *separate*, recycled value used only by
the JS viewer to build interactive nesting (`buildSpanData`'s
`childrenByParent`, `getDepth`, `selectSpanRenderSet` — all cycle-safe). The
folder ignores it.

**Legacy parentage is weaker.** The legacy path rebuilds the parent uid from the
enter's `parent_span_id` plus the parent's *first-enter timestamp*
(`legacy.rs`): `synthetic_instance_id(parent_span_id, parent_first_enter_ts)`.
If the parent is not present in the same file, that timestamp defaults to 0 and
the synthesized uid will not match the parent's real row — so legacy cross-file
parentage is unreliable. This is another reason new producers should emit the
modern dialect.

**Persistence vs. use.** `parent_span_uid` is written to the `spans/` Parquet as
a nullable column (`parquet_writer.rs`), but `/api/span-stats` groups by
`span_type_uid` and does **not** reconstruct parent trees today. Tree/nesting
reconstruction is currently a viewer-side feature over raw enter/exit. Setting
parentage correctly is therefore about interactive-viewer fidelity and future
tree analysis, not about the current aggregation flamegraph.

**Computing the parent (task-keyed).** The current tracing layer records a parent
only from an *explicit* `attrs.parent()`, so in practice `parent_span_instance_id`
is almost always `None`. Computing it correctly is possible — but **not from
thread state**. The parent of a span is "the span logically active *in the same
task* when the child was created," and in an async runtime the OS thread
interleaves tasks and tasks migrate workers, so "what span is entered on this
thread right now" bleeds across tasks. That is why inferring a contextual parent
from a thread-local (or from `tracing`'s thread-scoped current-span) is
unreliable.

The fix is to key the current-span stack by **task**, not thread. Tokio exposes
the current task anywhere inside a poll via `tokio::task::try_id()` (dial9 already
uses it for the wake graph, `traced.rs`). A producer maintains a per-task stack —
`HashMap<TaskId, Vec<span_instance_id>>`, or a `tokio::task_local!` stack —
pushing on enter, popping on exit, and at creation sets the parent to the top of
the *creating task's* stack. This is correct where a thread-local is not:

- **Interleaving:** task B's new spans consult B's stack, never A's, even on the
  same worker.
- **Worker migration:** the key is the (stable) task id, so a push on one worker
  and pop on another still balance.
- **A guard held across `.await`:** the held span stays in A's stack; when B runs
  (`try_id() == B`) it is unaffected. This is precisely the case the current
  layer's comment flags as unreliable — keying by task removes the hazard.

Two residual gaps remain in *any* implementation:

- **Off-runtime spans** (`try_id() == None`): no task, hence no async parent —
  genuinely unknown, and correctly left `None`.
- **Cross-`spawn` causal parentage:** a spawned task is a *new* task with an
  empty stack, so a span created inside the child's first poll has no parent to
  find. Task-locals do **not** inherit across `spawn`; the parent must be
  captured at spawn time and propagated. dial9 has a backstop others lack — the
  recorded spawn/wake graph (`waker_task_id`) can bridge task→task when the span
  stack cannot.

The tracing layer can adopt this with **no wire-format change** — only the source
of `parent_span_instance_id` changes (from `attrs.parent()` to the task-keyed
stack top). A purpose-built span system can go further: a task-local stack plus
`spawn` that propagates the current span **and** a `trace_id`/`root_span_uid`,
which is what turns a "compare sub-spans across request types" analysis from a
cross-file inference into a `GROUP BY trace_id` (the design reserves the nullable
OTLP `trace_id`/`parent_span_id` columns for exactly this).

> **Rule P8 (parentage):** On the close event, set `parent_span_instance_id` to
> the parent's stable instance id when a parent exists; leave it `None` for a
> root (never fabricate a parent). Compute the parent from a **task-keyed**
> current-span stack (`tokio::task::try_id()`), not a thread-local. Optionally
> set the enter event's
> `parent_span_id` to the parent's wire/span id for interactive-viewer nesting.
> Do not rely on dial9 to infer parentage from containment. Avoid self-parent
> and cycles.

### 4.7 Task association (`task_id` is inferred, never written)

A span carries **no `task_id` field**. The owning Tokio task is *inferred* from
the poll timeline: `task_id` lives only on `PollStart` events (`polls.rs`), and
`resolve_span_task(worker_polls, enter_ts)` binary-searches the **enter
worker's** polls for the one whose `[start, end)` covers the span's **enter
timestamp**, taking that poll's `task_id` (`legacy.rs`; the JS viewer's
`resolveSpanTask` does the same from `(segment.worker_id, segment.start)`).

This is why the enter event's `worker_id` is **not cosmetic**: it selects which
worker's poll lane to search. Without it — or when the enter does not fall inside
any poll (e.g. a span entered on a non-runtime thread) — the span resolves to
*no task*, and on-CPU/async-wait cannot be split (they stay `unknown_ns`, per
ADR-0002).

Reader state, as of this writing:

- **JS viewer** and the **legacy Rust folder path** both perform this
  poll-overlap inference — to split on-CPU vs. async-wait and to reconstruct
  per-poll active segments.
- The **modern Rust folder path** currently **defers** task-based attribution:
  `modern.rs` leaves `on_cpu_ns_est`/`async_wait_ns` as `None` (all elapsed falls
  into `unknown_ns`). It still attributes CPU/sched samples to entered intervals
  in stage 3 (§4.5). Task-based time splitting for modern spans is a future
  stage; when added it will reuse the same poll-overlap inference and therefore
  the same enter-`worker_id` requirement.

> **Rule P9 (task association):** Do not put `task_id` on span events. To make a
> span attributable to its task, emit its enter events with the correct
> `worker_id` and ensure enter/exit bracket on-CPU execution **inside a poll**
> (i.e. instrument work running on the runtime, not detached threads).

---

## 5. Legacy dialect (read-only compatibility)

Old producers emit:

- `SpanEnter:{target}::{name}:{file}:{line}` with fields `worker_id`, `span_id`,
  `parent_span_id`, `span_name` (no `span_instance_id`, no `tid`);
- `SpanExit:{…}` similarly;
- `SpanCloseEvent` with only `span_id`.

Reconstruction rules (`spans/legacy.rs`), documented so the contract is complete
— **do not target this format in new producers**:

- Pair enter/exit by **`span_id` alone**, *not* `(span_id, worker_id)`: async
  tasks migrate workers across `.await`, and worker-keyed pairing dropped ~44% of
  fully-captured spans on a measured beta trace.
- Synthesize a deterministic instance id: `BLAKE3("legacy_instance" ‖ span_id ‖
  first_enter_ts)[0..8]`. Same-`span_id` events within one segment are merged
  into one instance (ids are recycled per process, but rarely within a ~60s
  segment).
- Parse `target`/`name`/`file`/`line` from the schema name
  (`events.rs::parse_legacy_span_schema_name`). The `SpanEnter__{Type}`
  struct-derived form yields only the `{Type}` suffix (no target/file/line).
- Lifecycle start = first observed enter (conservative); `identity_quality =
  "legacy"`; `details_complete = false`.
- On-CPU vs async-wait for a legacy span is *estimated* by resolving the owning
  Tokio task (the poll on the enter worker covering the enter timestamp) and
  intersecting entered intervals with that task's polls.

---

## 6. Producing spans without the tracing layer

### 6.1 The genuinely minimal case

The smallest thing that produces a span row is **one close event with four
fields**. No enter/exit, no flags, no attributes:

```rust,ignore
use dial9_trace_format::TraceEvent;
use dial9_tokio_telemetry::telemetry::{clock_monotonic_ns, Dial9Handle};

#[derive(TraceEvent)]
struct SpanClose__MyOp {
    #[traceevent(timestamp)] timestamp_ns: u64, // close time
    span_instance_id: u64,                       // > 0, never recycled
    start_timestamp_ns: u64,                     // lifecycle start
    span_name: String,
}

# fn emit(handle: &Dial9Handle, id: u64, start_ns: u64) {
handle.record_event(SpanClose__MyOp {
    timestamp_ns: clock_monotonic_ns(),
    span_instance_id: id,
    start_timestamp_ns: start_ns,
    span_name: "my_op".into(),
});
# }
```

That yields a span with correct `elapsed_ns` and identity, but **no execution
detail**: no entered intervals, so no CPU samples attach and all elapsed time is
`unknown_ns`. `target`/`file`/`line` are empty (weaker `span_type_uid`
grouping), and it's never `details_complete`. This is enough for "show me this
operation's duration"; it is not enough for a flamegraph.

### 6.2 Adding on-CPU detail

To earn on-CPU attribution, bracket the on-CPU work with a balanced enter/exit
pair on the same `(span_instance_id, tid)`, and give the enter a `worker_id` for
task inference (§4.7):

```rust,ignore
use dial9_tokio_telemetry::telemetry::{clock_monotonic_ns, current_worker_id, Dial9Handle};
// `tid` is dial9's per-thread id; the public function is
// `dial9_core::thread::current_tid()`.
use dial9_core::thread::current_tid;

#[derive(TraceEvent)]
struct SpanEnter__MyOp {
    #[traceevent(timestamp)] timestamp_ns: u64,
    span_instance_id: u64,   // the pairing lane
    tid: u64,                // must match the exit on this thread
    worker_id: u64,          // enables task inference; see §4.7
}

#[derive(TraceEvent)]
struct SpanExit__MyOp {
    #[traceevent(timestamp)] timestamp_ns: u64,
    span_instance_id: u64,
    tid: u64,
}
```

Emit `SpanEnter__MyOp` when the operation goes on CPU and `SpanExit__MyOp` when
it yields, once per on-CPU stretch, then the close from §6.1 at the end. That is
the complete conforming producer. `span_instance_id` comes from a process-local
`AtomicU64` starting at 1 (0 is the disabled sentinel); allocate one per
instance and never recycle it.

### 6.3 Optional richer close fields

Everything below `span_name` on the close is `#[serde(default)]` — add a field
only when you have something real to say:

- `target` / `file` / `line` — sharpen `span_type_uid` grouping and give the UI
  a callsite. Recommended; keep them stable per span type (P5).
- `active_ns` — producer-accumulated active **thread** time. Omit (0) and the
  folder computes it from the local intervals. Only report it if you track it
  yourself (e.g. the span's enter/exit span multiple files).
- `parent_span_instance_id` — the parent's stable instance id (§4.6).
- `attributes: Vec<(String, String)>` — see §6.5.
- `unbalanced_enters` — see §6.4.

### 6.4 The one quality flag: `unbalanced_enters`

It defaults to `0` ("balanced / nothing to report"). A producer that emits
balanced enter/exit pairs never sets it.

**`unbalanced_enters`** is the producer's own count of enter/exit imbalance
observed for this instance at close (in the tracing layer: enters still open plus
exits that never matched an enter). It is a *quality flag*, not a requirement:
the decoder independently detects imbalance from the actual enter/exit events it
sees in the file and **adds** your number to its own (`modern.rs`). You only need
to report it for imbalance the decoder cannot see — e.g. enters/exits that
happened in an earlier, un-folded file. Non-zero degrades `details_complete`.

**Why `concurrent`, `saturated`, and `loss_observable` are not producer flags.**
Earlier drafts had four flags; three were dropped because a producer either
cannot honestly set them or the decoder already knows:

- **`loss_observable`** required the producer to know whether any of the span's
  enter/exit events were *dropped* from the buffer. On the thread-local buffer
  path there is no such signal — `record_event` is fire-and-forget, and nothing
  reports a per-stream drop. Observing drops would need per-thread sequence
  numbers (decoder spots a gap) or a buffer drop counter surfaced in segment
  metadata; neither exists for spans. So the flag was always `0`, and because
  `details_complete` *required* it, it was permanently forcing spans to
  incomplete. Removing it lets `details_complete` actually be reached.
- **`concurrent`** is fully derivable by the decoder (summed thread-time exceeds
  wall-time), so a producer-emitted flag was redundant. It remains as a *derived
  output* only (§4.3).
- **`saturated`** flagged a `u64`-nanosecond overflow of `active_ns` — roughly
  584 years of accumulated active time. It could never realistically be set and
  only mattered if the producer reported `active_ns` at all.

### 6.5 Attributes: the `attributes` Vec vs. top-level fields

**Today, the folder reads span attributes only from the dedicated
`attributes: Vec<(String, String)>` field on the close event.** Extra *top-level*
fields you add to a `SpanClose__` struct are **silently ignored** by the folder:
serde deserializes `SpanCloseSummary` by named field and drops unknown ones, so a
top-level `status_code: u32` never reaches `ResolvedSpan.attributes` or the
`/api/span-stats` facets. If you want an attribute to appear in span stats, it
must go in the `attributes` Vec.

This is asymmetric with enter/exit, and worth understanding:

- On **enter/exit**, the tracing layer emits user fields as **top-level**
  pooled-string fields, and the **JS viewer** collects everything outside the
  base field set (`BASE_ENTER_FIELDS`) as the span's `fields`. But the **Rust
  folder ignores enter/exit user fields entirely** (it reads only
  `timestamp`/`instance_id`/`tid`). So top-level enter/exit fields reach the
  interactive viewer, not the aggregation.
- On **close**, attributes are the folder's channel, and they must be the Vec.

So the honest answer to "couldn't attributes be top-level fields?": for the
interactive viewer, enter/exit already work that way. For the aggregation folder
(the thing that powers span-stats and facets), **not currently** — the close
decoder is keyed on the `attributes` field. Making the close decoder collect
unknown top-level fields as attributes (mirroring the JS viewer's enter/exit
convention) would let struct-derived producers use typed top-level fields and
keep native types; it is a reasonable decoder change but is **not implemented
today**. Until then, use the Vec.

### 6.6 Clock requirements

Timestamps must come from `clock_monotonic_ns()` (the same clock as poll/wake
events). The trace must also contain `ClockSync` events (dial9 emits these
automatically) so the folder can convert to wall clock and establish the file
boundary; without a clock offset the monotonic values are used raw.

---

## 7. Rules checklist

A producer conforms if all of the following hold:

- **P1** Struct names are `SpanEnter__{Type}` / `SpanExit__{Type}` /
  `SpanClose__{Type}`, sharing one `{Type}` per logical span type.
- **P2** Enter and its matching exit carry the **same** `span_instance_id` and
  `tid`; enter is emitted before exit; thread migration is modeled as exit-then-enter.
- **P3** Exactly one close event per instance at lifecycle end, with
  `timestamp_ns`, `span_instance_id (>0)`, `start_timestamp_ns`, `span_name`, and
  truthful quality flags.
- **P4** `span_instance_id` is process-local, monotonic, never recycled, nonzero.
- **P5** `name`/`target`/`file`/`line` are stable across instances of a type
  (so `span_type_uid` groups them).
- **P6** Timestamps are `clock_monotonic_ns`; `ClockSync` events are present.
- **P7** `unbalanced_enters` defaults to 0; set it only for cross-file imbalance
  the decoder cannot see (§6.4). Never inflate `active_ns`.
- **P8** Set `parent_span_instance_id` on the close to the parent's stable
  instance id when a parent exists; `None` for roots; never infer or fabricate.
  See §4.6.
- **P9** Do not write `task_id` on span events — it is inferred from the poll
  timeline via the enter's `worker_id`. Emit enter events with the correct
  `worker_id` and bracket on-CPU execution inside a poll. See §4.7.

## 8. Compatibility rules

- Adding fields to any span event is wire-compatible (self-describing schema).
  Old traces omit new fields; the folder's serde structs default them, and the JS
  viewer must tolerate `undefined` (see repo `AGENTS.md`).
- Removing a non-optional field is **not** safe for old-trace reads.
- Changing folded/derived output increments `SAMPLES_FORMAT_VERSION`, forcing a
  lazy refold; the wire contract itself is unversioned and additive.
- Malformed span events are counted and skipped with rate-limited logging; they
  never abort a fold.
</content>
</invoke>
