# Diff-first unified view

## Overview

Today the dial9 viewer has three separate analysis surfaces — the flamegraph,
the span explorer (`/api/span-stats`), and tokio stats (`/api/tokio-stats`) —
on three pages linked only by URL query strings, with one-directional deep
links between them. A minimal two-sided differential flamegraph exists
(`?diff=1` in `flamegraph.html`), but diffing lives only there.

This design makes **comparison the primary paradigm**. Every analysis surface
is built to compare two *selections*, **A** (the baseline) and **B** (the focus
/ anomaly). Looking at a single slice of data is the special case where **B is
empty** — a diff against "a perfect world where everything was infinitely
fast/cheap." The three surfaces stop being separate pages and become **views**
over one shared selection: switching from the flamegraph view to the spans view
re-runs the *same* A/B selection against a different endpoint.

The headline capability is to **pull out the most significant differences**
between A and B — "p99 of `db.query` went 12ms → 340ms", "this frame didn't
exist in A but dominates B", "on-cpu time drained into lock contention" — and
to do so honestly under demand-driven partial folds.

This is the presentation/analysis layer built on top of
[generalized-span-flamegraphs.md](./generalized-span-flamegraphs.md) (the data
layer: producer events, folded Parquet schema, wall-clock attribution, and the
`/api/span-stats` query interface). Domain vocabulary is defined in
[`CONTEXT.md`](../../CONTEXT.md) ("Unified diff-first view" section); this doc
is the narrative spec. Key decisions are recorded in
[ADR-0004](../adr/0004-diff-is-computed-server-side.md) and
[ADR-0005](../adr/0005-span-type-identity-excludes-metadata.md). A clickable
mock lives at `docs/ui-inventory/mocks/concept-diff-first.html`.

## Goals

- Make **comparison the default paradigm**: one page, one renderer per view,
  always speaking in terms of A vs B; a single slice is B = ∅.
- Let a user build A and B as **arbitrary shared filter sets**, and refine B
  from A with a single "Compare" action ("pop out > p99", "pop out this span").
- **Surface the most significant differences** per view, ranked and legible.
- Show the *difference*, not "here is A / here is B": every quantity renders
  through a small, reused **diff vocabulary**.
- Be **honest under partial folds**: suppress rankings until A and B reach
  comparable coverage; never present sampling noise as a headline finding.
- Analyze polls, worker-active intervals, and tracing spans through **one
  uniform in-code span model** and one set of analysis machinery.
- Keep the whole comparison a **shareable, self-contained URL**.
- Grow incrementally from the existing span explorer without deleting the
  current pages before parity.

## Non-goals

- A literal always-two-panels UI (the one-sided case is a single full-width
  panel; "B = ∅" is a conceptual unification, not a permanent empty pane).
- A unified cross-view "what changed" ranking in v1 (deferred — needs a common
  significance metric across heterogeneous deltas).
- Genuine per-instance correlation ("elapsed correlates with `db_time`
  span-by-span") in v1 (deferred — needs per-span joins that fight the
  source-file-bounded model).
- Global cross-file span reconstruction (inherited non-goal from the
  generalized-span design; duration decomposition stays source-file-bounded).
- Merging tracing spans and tokio tasks into one on-disk table or one on-screen
  table.
- Asserting causation. We surface differences ("auto-(not-quite-)root-cause"),
  we do not claim root cause.

---

## 1. The paradigm

### 1.1 Diffability

Every view compares two **selections**, A and B. A single-slice view is the
special case B = ∅. We strive for a **single-view** experience: one page, one
renderer per view, always in terms of A vs B. Legacy single-slice pages are not
deleted on day one, but new work is designed diff-first.

"Comparison" is **not a mode** — it is the paradigm. Avoid calling it a
comparison mode.

### 1.2 One renderer, two layout modes

"B = ∅" is the *conceptual* unification (same data model, same renderer code,
B's filter set merely empty) — **not** a literal always-two-panels UI:

- **Inspect** (B = ∅) renders a **single full-width panel** with all rich
  features.
- **Compare** transitions it into the **split** two-panel layout by revealing
  the B panel.

An always-present empty/ghost B panel is rejected — it would waste the viewport
the UX audit already flags as too empty on cold open. One renderer, one
selection model, two layouts.

---

## 2. Selection model

### 2.1 Selection

A **selection** (A or B) is a **shared filter set** — a predicate over the
trace data — applied uniformly across every view. It would make no sense to
change the filter criteria for just one view. The filter set is the *union*
across views: one selection can carry a `span_type_uid`, a duration band, a
`thread_class`, and an attribute-equality filter at once, and each view uses the
subset it understands.

The same set of fields plays two roles. As a **predicate** a field filters
(`service = Foo`, duration `> X`). As a **consequent** the same field is a
computed result of the selection ("given that predicate, p99 is now X"). The
diff highlights how a consequent changed between A and B.

### 2.2 Building A: the baseline

The **baseline** (selection A) is built in the viewer / S3 browser by
quick-click buttons — "whole service", "whole host" (new; these do not exist
today) — or today's drag-select. From the baseline you can **Inspect**
(one-sided) or **Compare**.

### 2.3 Building B: Compare (pop-out)

**Compare** creates selection B from the baseline. Any criterion you could
otherwise apply to narrow the current view is instead "popped out": rather than
narrowing in place, it **splits** into A vs B, where **B gets the popped
predicate `P`**.

By default the two sides are **disjoint**: B = baseline ∩ `P`, and A gets a
visible `NOT P` filter chip auto-added so A = baseline ∖ `P` — "the matching
subset vs the rest." This maximizes the significant-difference signal (comparing
a tail against a population that *includes* it dilutes the contrast). The
`NOT P` chip is **not a hidden mode**: it is an ordinary filter on A, shown like
any other; clicking it away reverts to "B vs the whole baseline" (A ⊇ B).

So B is normally a *refinement* of A's predicate, expressed entirely in the one
uniform filter model. A general filter-builder (arbitrary independent A and B)
still exists and is primary; pop-out is the common path. The existing "Add to
diff" capture tray and the derive-B presets (same-scope-different-host,
time-shifted) are demoted to accelerators that *populate* the builder.

### 2.4 Derived-statistic filters (e.g. "> p99")

A pop-out predicate over a *computed aggregate* (a percentile) is
coverage-dependent — it is computed over whatever is folded so far, unlike a
row-level predicate (`region = Bar`) which is stable as coverage grows.

On pop-out it is **frozen to a concrete, editable row predicate**
(`duration_ns > 340ms`, the p99 at that moment) so it behaves like every other
filter — single-pass scan, coverage-symmetry gating, and the `NOT P` complement
all just work. The chip **tracks its provenance** ("derived: p99 @ cov 12/480")
so a **refresh** recomputes the threshold against all currently-folded data.
Frozen for stability by default; re-derivable on demand. A *live* percentile
filter is rejected — its membership would churn as files land, fighting
coverage-symmetry and jittering the ranking.

### 2.5 "Everything" is never literally 100%

The canonical "B is everything" is **always a cap-bounded, representative
sample** — nothing in the system is ever literally 100% of the data (the
sampling cap and order key guarantee a uniform spread; the coverage badge tells
the truth, e.g. "100 / 45,000 files, 0.2%"). In the pop-out flow, "everything"
means A's **coarse scope minus the popped predicate** (the `NOT P` complement
within A's time/service window) — bounded by construction and the more useful
comparison (it controls for time-of-day and deploy version; the literal
all-time fleet would confound). A genuinely fleet-wide B is an explicit,
clearly-labeled choice, not the default.

### 2.6 What carries when switching views

- **Semantic** state — any filter, a focused span type, a time sub-range — is
  *just a filter*, lives in the selection, and carries across views
  automatically. Selecting span `auth` in the spans view and switching to the
  flamegraph scopes the flamegraph to `auth`; this falls out of the selection
  model, it is not a special cross-view rule.
- **Navigation** state — flamegraph zoom rectangle, butterfly/inspect root,
  exact search string, histogram brush geometry — is purely mechanical, stays
  **per-view**, and is remembered per-view so switching away and back restores
  your place.

### 2.7 View-local filters

A view may *add* filter criteria on top of the shared selection, in that view's
own terms — e.g. right-clicking a flamegraph frame to keep only samples through
that code path. A view-local filter can target **A only, B only, or both** —
all three are legitimate (both = keep the sides honestly comparable; one side =
ask a lopsided question). When a filter targets only one side, A and B are no
longer defined by the same predicate, and the UI **must make that asymmetry
visible rather than silent**. View-local filters are undoable via a
breadcrumb/zoom history.

---

## 3. The diff vocabulary

Showing "here is A" beside "here is B" is not showing the difference — it makes
the reader compute the diff by eye. Instead, every quantity is one of a few
**diffable types**, and each type has **one canonical A-vs-B rendering**, reused
across all views so a p99 diff looks and behaves the same in the spans table, a
flamegraph tooltip, and the significant-difference rail.

| Diffable type | Examples | Canonical diff rendering |
|---|---|---|
| **Scalar** | p50/p95/p99, count, mean poll, a `db_time` metric | **Dumbbell** — A-dot and B-dot on one shared (log) axis joined by a colored bar; length + direction = the change |
| **Distribution** | a span type's duration histogram | **Overlaid histograms** on a shared log axis, with **p50 / p99 / p999 markers** that move with the distribution (line-style encodes the percentile: solid p50, dashed p99, dotted p999; color encodes the side). Shows the whole shift, not three numbers |
| **Composition (reallocation)** | the five-way time breakdown; nested child-span time | Two stacked bars, **A over B**, segments labeled inline with category **and the time they represent** (pct × that side's p99 elapsed); a one-line note names the biggest mover |
| **Set membership** | which span types / frames exist | **new / gone** badges |
| **Tree** | call graph; nested time-attribution | **delta-colored flame / icicle** |

**Consequence for tabular views:** a spans or tokio view is **not two tables**.
It is **one aligned table** — one row per identity (union of A and B), each cell
rendering its quantity's diff primitive. **The row is the diff.** Sorting by
change-magnitude merges the aligned table with the significant-difference rail.
The rail derives from the same numbers as the table, so they never disagree.

Avoid side-by-side A/B tables — that is "show both", the failure mode this
vocabulary exists to kill.

### 3.1 Time-attribution is a metric bag, not necessarily a tree

"Where did a span's elapsed time go" generalizes beyond the fixed five-way
breakdown (on-cpu / sync-block / async-wait / sched-delay / unknown). A span
carries a **bag of measured quantities**; the subset that are *durations*
(`db_time`, `on_cpu`, a child span's contribution) are what "composition" is
made of.

- A `db_time` **metric attached to the span** is the easy, always-available
  case → scalar dumbbell diff.
- A nested parent/child **tree** is the richer case when containment exists →
  delta-tree diff.

The diff engine does not distinguish them — it diffs numbers grouped by
identity; the tree is a *rendering* choice for when the numbers nest, not a
separate diffable type. So the fixed five-way bar is just the shallowest
projection. This lets "my p99 is slow *because* `db.query` is slow" read
straight out of the A-vs-B diff, since B is already the p99 tail.

Two honesty/scope constraints:

- **Mandatory `unknown`.** Any duration decomposition is source-file-bounded
  (no global span-graph reconstruction). It must always carry an explicit
  `unknown` node and never be fabricated to sum to 100%. The on-cpu portion
  falls out of the enriched sample scan (`enclosing_spans` depth 0–2); the
  by-child *wall-time* split is the bounded part.
- **Aggregate diff ≠ per-instance correlation.** The A-vs-B diff gives the
  aggregate claim ("the slow bucket spends proportionally more time in
  `db.query`"), which needs no new machinery because B = the tail. Genuine
  per-instance correlation is a separate, deferred capability.

### 3.2 Unsymbolized frames excluded from diff

Frames whose addresses did not resolve to a symbol are **excluded** from diff
coloring and the significant-difference listing. Unsymbolized frames will
essentially always differ between A and B (different raw addresses), so
including them would flood the ranking with noise that is not a real behavioral
difference.

---

## 4. Views

A **view** is one way of presenting a selection pair. Switching views re-runs
the same A/B selection against a different endpoint; the selection is unchanged.
Avoid calling views "lenses" (too close to "filter/predicate").

### 4.0 The time-navigation pane (shared shell chrome)

The unified shell embeds a **collapsible time-navigation pane** — a version of
the S3 browser's density heatmap — as shared chrome above/beside the views. It
serves two jobs that no other part of the UI does:

1. **It is where A and B are picked, and where you stay oriented in time.** The
   pane is the existing time × (service/host) density heatmap (X = wall-clock,
   rows = hosts, cells = bytes/time). You **drag window A, drag window B**; each
   drag is the selection the browser already computes and encodes as
   `start_ns`/`end_ns`. A and B show as two labeled brushed bands on the same
   axis. Without this, a selection is an abstract filter set with no "where am I
   in time" anchor; with it, the load-test-window comparison (start vs end) is a
   direct gesture. The pane collapses once the windows are set.
2. **It is where exemplars land back.** Each span type's slow `Exemplar`s carry
   `start_ns` + `host`; overlaying them as markers on the heatmap's time axis
   (row matched by host) shows *when and where* the anomalies happen — and,
   because they sit on the A/B bands, that the slow tail clusters in B, not A.
   Selecting a span-type row highlights its exemplars in the pane; clicking a
   marker deep-links to that exact span.

The pane reuses the pure `heatmap.js` core unchanged; the renderer/interaction
(currently inlined in `index.html`) is extracted into a container-scoped
component so the browser and the pane share one implementation. No backend
change is needed for the exemplar overlay — the time/host fields already exist
on `Exemplar`.

### 4.1 Flamegraph view

The diff capability is folded **into** the feature-rich flamegraph renderer
(search, inspect/butterfly, zoom history) — not built on top of the minimal
two-sided diff renderer. Layout is **two synced rich panels**: A left, B right,
each the full renderer, kept in sync — zoom in one zooms the other to the same
frame path, search highlights matches in both, inspect/butterfly roots both at
the same frame. That synchronization is what makes it "one view of two things"
rather than two independent flamegraphs.

Each existing feature grows a side-by-side variant: e.g. search shows the
matched field in *each* selection with each side's representative percentage.
The color encoding is **user-selectable** (delta / hotness / by-module), not a
fixed hotness scale; in the B = ∅ case the delta coloring and the "A heavier ↔
B heavier" ramp are meaningless and are replaced by a neutral single-selection
coloring. Alongside the panels is a **listing table** of the biggest
differences, ranked by both **percentage** delta and **absolute sample-count**
delta — the flamegraph view's instance of the significant-difference feature.

A single merged delta-colored tree is a possible later toggle, not the primary
layout (self-normalized side-by-side widths are more legible than delta-color on
one tree).

### 4.2 Spans view

One aligned diff table (§3), one row per span type (union of A and B). Cells:
occurrences (A→B), duration **distribution** (overlaid histograms + p50/p99/p999
markers), and **composition** (A-over-B stacked bars, inline-labeled with time),
plus new/gone membership badges. The catalog groups by span type but can group
by any attribute (§4.4).

### 4.3 Tokio view

The tokio view is the spans view of `kind = tokio_poll` **grouped by spawn
location** — the old tokio-stats shape, produced by the same machinery. Poll
durations render with the same distribution + composition primitives;
on-cpu/off-cpu is the five-way composition; the old top-N "longest polls" list
is the span view's slow-exemplars.

### 4.4 Group-by-attribute

A span type is **not** subdivided by its metadata: all polls are one type
(`kind = tokio_poll`). Instead, spawn location is typed metadata and the span
machinery has a first-class **group-by-attribute** capability — the tokio view
is "spans of kind `tokio_poll`, grouped by spawn location." This generalizes:
tracing spans can group by any attribute (e.g. `handle_request` by `route`).
Grouping is a runtime operation over metadata, not baked identity. This
**departs from** generalized-span §1.1 (which baked spawn location into the type
UID); see [ADR-0005](../adr/0005-span-type-identity-excludes-metadata.md).

---

## 5. Span-kind unification

Polls (`PollStart`/`PollEnd`), worker-active intervals
(`WorkerUnpark`/`WorkerPark`), and tracing spans are all analyzed as **spans**.
The unification is at the level of *in-code representation and analysis
machinery*, not storage and not presentation:

- **Storage stays separate.** The physical `polls/` projection and the `spans/`
  table remain distinct — polls are the highest-volume event class, and folding
  every poll into a full span row is a deferred storage cost (see the
  generalized-span design). The worker-active interval is a further span *kind*.
- **In-code representation is uniform.** After leaving Parquet, every kind
  deserializes into one span type. Kind-specific metadata (e.g. tokio spawn
  location) rides as typed fields — special-cased decoding
  (`derive(Deserialize)`-style), not a separate code path.
  `kind ∈ {tracing, tokio_poll, worker_park}`.
- **Analysis machinery is shared.** Anything computing span lengths,
  frequencies, percentiles, or composition runs over any kind. If poll times for
  one spawn location are far longer than their peers, that is *interesting* and
  it is found by the *same code* that ranks tracing spans.
- **Worker-active span** = the `[WorkerUnpark, WorkerPark)` interval during
  which a worker is **busy** (running tasks), *not* idle. Same polarity as a
  poll: more of them, or longer ones, means the runtime is busier. (This is the
  interval CONTEXT's Worker attribution section calls the "active span".)
- **Presentation stays separate.** It does not make sense to list tracing spans
  and tokio tasks in the same view table. The view groups by kind; the shared
  machinery is what makes cross-kind observations possible.

---

## 6. Significant differences

The "pull out the most significant differences" feature is ranked
**per-view**, in that view's own units:

- flamegraph: |Δ self-fraction| of a frame, including frames present on only one
  side;
- spans: |Δ p99| / |Δ mean| of a span type;
- tokio: |Δ mean poll| / |Δ p99| of a spawn location.

A unified **cross-view** ranking — one "what changed" summary drawing the
globally most significant deltas from every view — is the aspiration but is
**deferred**: it needs a common significance metric to compare heterogeneous
deltas (Δp99-of-a-span vs Δself-fraction-of-a-frame), which is unsolved. Ship
per-view first, learn what "significant" means empirically, then attempt the
summary.

We surface differences; we do not assert causation.

---

## 7. Honesty under partial folds

### 7.1 Coverage symmetry

A/B differences are only trustworthy when both selections have folded a
*comparable* fraction of their matched set. Comparing a 5%-folded A against a
90%-folded B produces an illusion — B appears to "have" frames A is merely
missing.

The unified view drives both sides toward comparable sampling caps and
**suppresses / grays the significant-difference ranking** until both sides
clear a minimum coverage, showing an explicit "still folding — differences
preliminary" state instead. This is the v1 guard.

Raw-magnitude ranking with only a coverage badge is explicitly **rejected**: an
auto-surfaced "top difference" that is sampling noise destroys trust on first
use.

### 7.2 Confidence intervals (end-state)

The honest end-state is **per-difference confidence intervals** — the planned
per-node CIs that tighten as files fold. Rank by statistical significance, not
raw magnitude, so a frame seen once in A and zero times in B sinks rather than
headlines. This is a substantial statistics effort and is not a v1 blocker.

---

## 8. API design

The diff and the ranked significant-difference list are computed
**server-side**, not in the browser
([ADR-0004](../adr/0004-diff-is-computed-server-side.md)). The overriding
design constraint is that **the current three endpoints already share almost
everything the compare mode needs** — so compare is mostly *reuse* plus a thin
envelope, not a new subsystem.

### 8.1 What matches today's APIs (reused unchanged)

The three GET SSE endpoints today are `/api/flamegraph`, `/api/span-stats`,
`/api/tokio-stats`. They already share:

| Shared piece | Today | In compare |
|---|---|---|
| **`Scope`** (`start_ns`, `end_ns`, `service`, `hosts[]`) — `ingest/aggregate.rs` | one per request | one **per side**; a selection's scope subset maps 1:1 |
| **`AggContext`** (`bucket`/`prefix`/`aws_region`) | resolved from params | unchanged — both sides share one context |
| **`resolve(agg, scope, RefineOpts{max_files}) → Resolved`** — `ingest/refine.rs` | called once | called **once per side**; each side's `Resolved.folded` + `files_matched` is exactly the coverage-symmetry input |
| **`fold_stream::drive`** unfold driver — `server/fold_stream.rs` | drives one sink | drives **two** sinks (see §8.3) |
| **`Coverage`** block (incl. `folded_set_id` = blake3 of the folded-leaf set) | one per snapshot | one **per side**; `folded_set_id` is the symmetry signal |
| **Per-view filter params** (`thread_class`, `source`, `phase`, `spawn_location`, `span_type_uid`, `min_span_ns`/`max_span_ns`, `min_poll_ns`/`max_poll_ns`, `attr[]`, …) | on the request struct | **unchanged**; each side carries its own set |
| **Per-view response types** (`FlamegraphResponse`, `SpanStatsResponse`, `TokioStatsResponse`) | the SSE payload | reused verbatim as the `a` and `b` fields of the diff envelope (§8.4) |

The compare mode adds **no new scope/filter/resolve/coverage machinery** — it
composes the existing pieces.

### 8.2 The request shape: `compare` is a mode, not a new endpoint

Compare is a **mode on each existing endpoint**, keyed by a single new param
carrying selection B:

```
GET /api/span-stats?<A's params…>&b=<base64url(B's params)>
```

- **`b` absent → today's response, byte-for-byte unchanged.** This *is* the
  "B = ∅ is the special case" principle expressed in the API: the current
  single-selection endpoint is the compare endpoint with B omitted. No
  migration of existing callers.
- **`b` present →** B's params are the *same param struct* as the endpoint's own
  (`SpanStatsParams`, etc.), base64url-encoded — mirroring the existing client
  diff's `a=`/`b=` codec (`flamegraph_diff.js`). The **client** expands B's
  permalink-delta (§9: "B encoded as a delta from A") into a full param set
  before the call, so the **server stays dumb about the "B refines A"
  relationship** — it just resolves two independent selections. The `NOT P`
  complement on A is likewise just an ordinary filter already present in A's
  params.

Rejected alternative: a separate `/api/compare` route. It would duplicate every
endpoint's param parsing and validation and force the client to special-case
which endpoint's compare to call. A per-endpoint `b=` mode keeps validation and
the response-type family co-located with the single-selection handler.

### 8.3 The two genuinely new server behaviors

Everything else is composition; these two are new code:

1. **Coordinated fold scheduling.** Instead of two independent streams racing
   (what the client does today — the direct cause of coverage asymmetry), one
   driver interleaves both sides' fold work to keep
   `files_folded / files_matched` comparable within a tolerance (not rigid
   lockstep). Mechanically this is the existing `fold_stream::drive` unfold
   generalized to alternate between two `Resolved` work-lists / two sinks,
   emitting a combined snapshot after each step. Because A and B usually share
   most of their matched files (B refines A's predicate over the same coarse
   scope), the same folded part often feeds both sides — so a row-level B
   predicate can be evaluated by **partitioning one fold**, not double-folding.
2. **Server-side merge + rank.** The tree merge that lives in
   `flamegraph_diff.js` today moves into Rust, over the kind-agnostic span
   accumulator ([span-kind unification](#5-span-kind-unification)), producing
   the merged `diff` structure and the bounded `ranked` list. One
   implementation serves all three views.

### 8.4 The response envelope: `DiffSnapshot<T>`

Each SSE snapshot wraps the view's **existing** response type, adding the merge,
the ranking, and per-side coverage:

```rust
struct DiffSnapshot<T> {          // T = FlamegraphResponse | SpanStatsResponse | TokioStatsResponse
    a: T,                         // side A — the current response type, unchanged
    b: T,                         // side B — same type
    diff: T::Diff,                // merged structure (flamegraph: tree w/ per-node a/b counts;
                                  //   tabular: rows aligned by identity w/ a/b values)
    ranked: Vec<Difference>,      // bounded, sorted; the significant-difference list
    coverage_a: Coverage,         // reused Coverage struct, per side
    coverage_b: Coverage,
    comparable: bool,             // coverage-symmetry gate: are the two folded fractions close enough
                                  //   to trust `ranked`? Client grays the ranking until true.
}

struct Difference {
    id: String,                   // frame name / span_type_uid / spawn_loc
    kind: DiffKind,               // Frame | SpanType | SpawnLoc
    a_value: f64, b_value: f64,   // the compared consequent (self-fraction, p99, mean poll…)
    delta_pct: f64,               // relative change  (∞ / −100% encode new / gone)
    delta_abs: f64,               // absolute change (e.g. sample-count delta)
    membership: Membership,       // Both | OnlyA | OnlyB
}
```

- `a` and `b` are the **unchanged** per-view response types, so a client can
  render either panel with today's rendering code — the single-selection
  renderer *is* the per-panel renderer.
- `diff` is the merged form the two synced panels / the aligned table read from.
- `ranked` is bounded (top-N, unsymbolized frames excluded) and drives the
  significant-difference rail; it is derived from the same numbers as `diff`, so
  they never disagree.
- `comparable` is the coverage-symmetry gate (§7.1): the client shows the
  "still folding — differences preliminary" state while it is false.

### 8.5 Group-by-attribute (extends span-stats §5)

Independent of compare, the span-stats query gains a `group_by=<attribute-key>`
param (default: span-type identity). It regroups the same folded rows by any
metadata key (e.g. `group_by=spawn_location` is the tokio view;
`group_by=route` groups `handle_request` by route). This is the query-side of
[ADR-0005](../adr/0005-span-type-identity-excludes-metadata.md) — grouping is a
runtime operation, not baked into `span_type_uid`.

### 8.6 What the client keeps

The **client** renders the two synced panels + aligned table + rail, and owns
only interactive navigation (zoom / search / inspect) over the shipped
structure. The legacy `flamegraph_diff.js` client merge is superseded (kept, if
at all, as a thin fallback for the old `?diff=1` page until it is retired).

This section extends the query interface in generalized-span §5 with (a) the
group-by-attribute capability and (b) the compare mode + `DiffSnapshot`
envelope.

---

## 9. Shareable state

The whole comparison is a **shareable, self-contained URL** — required to not
regress the UX audit's task-blocking finding that view state lives only in
memory. Encoding:

- **A is encoded fully; B is encoded as a *delta* from A** (the popped
  predicate(s) + any auto-added `NOT P`), because B is normally a refinement of
  A. This keeps URLs compact as the filter set grows and makes "B is a
  refinement of A" explicit in the URL structure. A fully-independent B (rare)
  falls back to a full encode.
- Per-view navigation state, the active view, the color encoding, and
  derived-statistic-filter provenance ride as additional URL params.
- No server-side saved-selection storage — the model stays URL-self-contained
  (credentials in `sessionStorage`, everything else in the URL).

---

## 10. Delivery plan

The vertical slices are specified in
[diff-first-delivery.md](./diff-first-delivery.md). Summary: a **general fold
scheduler is built first (S0)** — foundational infra so coverage symmetry is
correct from the first diff — then the diff capability lands as vertical slices
that each cut data → API → UI and end demoable: **S1** load-test window
comparison (spans p99), **S2** Compare/pop-out UX, **S3** full spans vocabulary,
**S4** flamegraph view diff, **S5** tokio via group-by-attribute.

The unified view is grown by **forking the current span explorer page** as the
host shell, then *adding* to it — not a greenfield rewrite and not
flamegraph-first. The span explorer is already closest to the destination: it
hits `/api/span-stats`, has the catalog → select → drill-into-flamegraph flow,
computes the percentiles/composition/exemplars that are the raw material for the
significant-difference feature, and owns the span→flamegraph deep link. Forking
protects the live page while building.

1. **Selection shell.** Add the A/B selection model + Compare pop-out (with the
   `NOT P` complement and derived-statistic freezing) to the fork; wire the
   shareable-URL codec.
2. **Spans diff.** Diff the span catalog the fork already renders — the aligned
   table (§3) and the spans-view significant-difference rail. Add
   group-by-attribute and the server-side compare mode (§8).
3. **Embed the flamegraph view.** Fold the rich flamegraph renderer in as a view
   *inside* the shell (two synced panels), so the flamegraph stops being a
   separate `window.open` page.
4. **Tokio view.** Group `tokio_poll` spans by spawn location; retain the
   `polls/` projection until measured evidence supports collapsing it.
5. **Coverage honesty.** Coverage-symmetry gating and the "differences
   preliminary" state; per-difference confidence intervals later.

The three existing pages (flamegraph, span_explorer, tokio_stats) stay alive and
**un-deprecated** until the unified view reaches per-view feature parity; a page
is deleted only when its unified equivalent is strictly better. The cross-view
significant-difference summary and per-instance correlation are deferred beyond
the first slices.

## Alternatives considered

### Literal single-renderer (diff-against-empty always)

Rejected as the *implementation*, kept as the *mental model*. Routing a plain
single flamegraph through the two-sided merge/color code would regress the
working single-view rendering, make "heavier in A vs B" coloring meaningless
against an empty side, and leave the significant-difference ranking undefined.
Instead: one renderer and one selection model, with two layout modes (§1.2).

### Client-side diff (extend `flamegraph_diff.js`)

Rejected for the general case (ADR-0004). Client-side ranking can only rank what
was shipped, reintroducing a sampling bias the server does not have, and would
require re-implementing merge+rank per view in JS.

### A separate `/api/compare` endpoint

Rejected (§8.2). It would duplicate every endpoint's param parsing and
validation and force the client to special-case which view's compare to call. A
per-endpoint `b=<base64>` **mode** keeps validation and the response-type family
co-located with the single-selection handler, and makes "B = ∅ → today's
response, unchanged" fall out for free (compare is the current endpoint with B
omitted). It also preserves backward compatibility: existing single-selection
callers are untouched.

### Spawn-location baked into the poll span-type identity

Rejected (ADR-0005). Baking one attribute into `span_type_uid` does not
generalize; a runtime group-by-attribute capability serves polls-by-spawn-loc
and tracing-spans-by-route with the same mechanism.

### Side-by-side A/B tables

Rejected (§3). Two tables make the reader compute the diff by eye; the diff
vocabulary renders the difference directly, with the row as the unit of diff.

### Unified cross-view significance ranking in v1

Deferred (§6). Comparing Δp99-of-a-span against Δself-fraction-of-a-frame needs
a common significance metric that does not yet exist; per-view ranking ships
first.
