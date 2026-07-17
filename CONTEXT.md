# Glossary

Project-wide vocabulary. Terms here are the canonical names used in code,
docs, issues, and PRs. If a term you need isn't here, either it's not yet
resolved (raise it) or you're inventing language the project doesn't use
(reconsider).

## Aggregation pipeline

**Source file**:
One immutable raw trace segment in S3
(`…/{date}/{HHMM}/{service}/{host}/{boot}/{ts}-{i}.bin.gz`). The atomic
unit of incremental aggregation work: ordered, fetched, decoded, and
folded into the `samples` table one at a time. Identified by its
`source_key`.
_Avoid_: segment (ambiguous with trace-format segments), object.

**Samples table**:
The facts table. One row per raw CPU sample (`timestamp_ns`, `stack_id`,
`thread_class`, `source`, `host`, …), stored as partitioned Parquet.
Aggregated (`GROUP BY stack_id`) at query time, never pre-folded — it
retains per-sample detail so the viewer can drill into any dimension. It
is populated *incrementally and partially* by demand-driven aggregation,
not batch-filled up front.
_Avoid_: facts table (use "samples table"), histogram store.

**Order key**:
`BLAKE3(ORDER_VERSION ++ source_key)`, ascending. A deterministic
pseudo-random total order over the [[source-file]] set that is uniform
across host and time, so the first K files are a representative spread of
the scope rather than one host's earliest minutes. Recomputed at query
time; produces no persisted artifact. "First K" is relative to the set a
query matches, so it shifts as new files land.
_Avoid_: shuffle, sort order.

**ORDER_VERSION**:
A baked constant in the hash input of the [[order-key]]. Bump it to change
the *scheduling* permutation. Lives **only** in the hash input — never in
an output path — because the samples cache is order-independent and must
survive a version bump untouched.

**SAMPLES_FORMAT_VERSION**:
A baked constant in the **output key path**
(`{output_prefix}/v{N}/samples/…`). Bump it when changing *what we
persist* and we want a deliberate recompute. The bump points reads/writes
at a fresh empty tree; files repopulate lazily on demand (no backfill
job). The old tree is abandoned and GC'd out-of-band.

**Refinement loop**:
The query control flow, realized as a **single held-open SSE stream** per
request. The server *resolves* the scope once (list → [[order-key]] sort →
retain the matched set + take the first [[sampling-cap]] files as the new-work
prefix → list the folded set), streams every folded matching part in bounded
cumulative snapshots, then folds missing prefix [[source-file]]s (in order)
concurrently and pushes a fresh tree + [[coverage]] block as each file lands,
closing when the work-list drains. The cap bounds new source folding; reusable
cache from overlapping scopes can make coverage exceed it. The
endpoint merges each folded file into an in-memory accumulator incrementally
(one merge per file), rather than re-scanning the whole folded set per event.
No background tasks and no coordination — folding runs only while the request
is open (the fold `JoinSet` is dropped, cancelling in-flight folds, when the
client disconnects), and re-folding is safe by idempotency. Coverage climbs
monotonically until cached reconstruction and the bounded work-list drain; it may
exceed the cap because the cap limits new source folding, not reusable cache.
(Deferred optimization: memoize each file's immutable `{stack_id→count}`
histogram *across requests* so a reopened stream sums cached per-file maps.)
_Avoid_: client re-polling, `refine=` request flag, job handle, background
daemon (all superseded by the stream).

**Coverage**:
How much of a query's matched scope has been folded so far, reported on
every [[refinement-loop]] response so users know how complete the view is.
v1 = file coverage: `{ files_matched, files_folded, samples_folded }`,
shown as e.g. "12 / 480 files (2.5%)". The literal statistical accuracy
(per-node confidence intervals that tighten as files fold) is a planned
later addition, not v1.
_Avoid_: progress, accuracy (reserve "accuracy" for the future per-node CIs).

**Fold**:
Decode one [[source-file]] and write its CPU samples as a deterministically
named part-file
(`samples/service=…/date=…/host=…/{blake3(source_key)}.parquet`). A
zero-sample file still writes an empty part-file. The part-file's
*existence* is the record that the file is folded — there is no manifest
and no skip-set (see ADR-0003). Re-folding writes the same key, so it is
idempotent.
_Avoid_: ingest (reserve for the optional out-of-scope warmer), process.

**Matched set / Folded set**:
**Matched set** = the [[source-file]]s a query's scope selects (a LIST of
the source prefixes); the [[coverage]] denominator and the [[order-key]]
input. **Folded set** = those already written to `samples` (a
scope-pruned LIST of the partitioned `samples/` tree). Coverage is their
intersection.

**Baseline floor**:
The minimum number of [[source-file]]s (in [[order-key]] order) a query
folds — the floor on the [[sampling-cap]], so even a tiny scope folds a
non-trivial sample. Default **4**, configurable. Small because each file is
~37–50 MB; the [[coverage]] label, not a large floor, is what keeps users
from over-trusting an early tree. The [[refinement-loop]] streams the rest.
Moot if a preload warmer is enabled.
_Avoid_: baseline sample count (it's files, not samples).

**Sampling cap**:
The point at which the [[refinement-loop]] stops folding a scope's tail:
`min(percentage × files_matched, absolute_ceiling)`, floored at the
[[baseline-floor]]. Defaults **5%** and **100 files**, both
backend-configurable. The percentage keeps small scopes sensible; the
absolute ceiling stops a fleet-day scope from chasing 5% of tens of
thousands of files (which would re-create the batch job). The
[[refinement-loop]] closes the stream once cached reconstruction and its missing
in-prefix work drain. A user-facing "fetch more" reopens the stream with a
raised new-work ceiling for that scope on demand. Folding also stops early if the
client disconnects (refine-while-watched). The cap is not a coverage ceiling:
matching reusable cache may take coverage beyond it, including to 100%.
_Avoid_: completion target for new folding (the cap bounds work, not coverage).

**Scope**:
A query's selection: a time range (minute precision), a service, and a
**host set** (the heatmap box can span many hosts → `host=` is repeatable;
empty = all hosts, each entry an exact match). Translated to the
[[matched-set]] in two stages: (1) a coarse S3 prefix prune over the
`{date}/{HHMM}/` rotation buckets the window spans, widened by one bucket
each side; (2) an in-memory interval-overlap filter on each file's filename
`epoch` (padded by the known raw-trace segment duration), plus the
service/host-set filter. Time/`HHMM` is high in the key so it prunes by
prefix; service/host is below it so it is an in-memory filter. The S3
browser's 🔥 button builds a scope from the heatmap selection's hosts +
`[t0,t1]` and drives the [[refinement-loop]] when the server advertises
`aggregation_enabled` via `/api/config`.
_Avoid_: query (use "scope" for the selection, "query" for the request).

## Unified diff-first view

**Diffability**:
The design principle that every analysis surface is built to compare two
[[selection]]s, A and B. A single-slice view is the special case where one
side is empty (a diff against "a perfect world where everything was
infinitely fast/cheap"). We strive for a **single-view** experience: one
page, one renderer per lens, that always speaks in terms of A vs B. Legacy
single-slice pages are not deleted on day one, but new work is designed
diff-first.
_Avoid_: comparison mode (it is not a mode; it is the paradigm).

**One renderer, two layout modes**:
"B = ∅" is the *conceptual* unification (same data model, same renderer code,
B's filter set merely empty) — **not** a literal always-two-panels UI. The
one-sided case ([[baseline]] Inspect) renders as a **single full-width panel**
with all rich features; **Compare** transitions it into the **split** two-panel
layout by revealing the B panel. An always-present empty/ghost B panel is
rejected — it would waste the viewport the UX audit already flags as too empty
on cold open. One renderer, one [[selection]] model, two layouts (full-width
when B empty, split when B populated).

**Renderer direction**:
The diff capability is folded **into** the feature-rich flamegraph renderer
(search, inspect/butterfly, zoom history), not built on top of the minimal
two-sided diff renderer. The layout is **two synced rich panels** — A left,
B right, each the *full* renderer (zoom, search, inspect all work per side),
kept in sync: zoom in one zooms the other to the same frame path, search
highlights matches in both, inspect/butterfly roots both at the same frame.
That synchronization is what makes it "one view of two things" rather than two
independent flamegraphs. Each existing feature grows a side-by-side variant:
e.g. flamegraph search shows the matched field in *each* selection with each
side's representative percentage. The color encoding is **user-selectable**
(not a fixed hotness scale). Alongside the panels is a **listing table** of
the biggest differences — ranked both by **percentage** delta and by
**absolute sample-count** delta — the flamegraph view's instance of
[[significant-difference]]. A single merged delta-colored tree is a possible
later toggle, not the primary layout (self-normalized side-by-side widths are
more legible than delta-color on one tree).

**Unsymbolized frames excluded from diff**:
Frames whose addresses did not resolve to a symbol are **excluded** from the
diff coloring and the [[significant-difference]] listing. Unsymbolized frames
will essentially always differ between A and B (different raw addresses), so
including them would flood the ranking with noise that is not a real
behavioral difference.

**Selection**:
One of the two things being compared: **A** or **B**. A selection is a
**shared filter set** — a predicate over the trace data — applied uniformly
across every [[view]]. It would make no sense to change the filter criteria
for just one view. The filter set is the *union* across views: a single
selection can carry a `span_type_uid`, a duration band, a `thread_class`, and
an attribute-equality filter all at once, and each view uses the subset it
understands. A single-slice display is the special case B = ∅.
_Avoid_: pane, side (use A/B), scope (scope is the coarse S3-prune subset of
a selection's filters — see the Aggregation glossary).

**View**:
One way of presenting a [[selection]] pair: the **flamegraph** view, the
**span-stats** view, the **tokio** view. Switching views re-runs the same A/B
[[selection]] against a different endpoint; the selection is unchanged.
_Avoid_: lens (too close to "filter/predicate"), drawer, tab, page.

**Derived-statistic filter**:
A pop-out predicate defined over a *computed aggregate* rather than a single
row — e.g. ">p99 duration". Unlike a row-level predicate (`region = Bar`,
stable as [[coverage]] grows), a percentile is coverage-dependent: computed
over whatever is folded so far. On pop-out it is **frozen to a concrete,
editable row predicate** (`duration_ns > 340ms` — the p99 value at that
moment) so it behaves like every other filter (single-pass scan,
[[coverage-symmetry]] gating, `NOT P` complement all just work). But the chip
**tracks its provenance** — "derived: p99 @ coverage 12/480" — so a
**refresh** recomputes the threshold against all currently-folded data. Frozen
for stability by default; re-derivable on demand.
_Avoid_: live percentile filter (the membership churns as files land, fighting
coverage-symmetry and jittering the [[significant-difference]] ranking).

**Semantic focus vs navigation state**:
The rule for what carries when switching [[view]]s. **Semantic** state — any
filter, a focused span type, a time sub-range — is *just a filter*, lives in
the [[selection]], and therefore carries across views automatically (selecting
span `auth` in span-stats and switching to the flamegraph scopes the
flamegraph to `auth` — this falls out of the selection model, it is not a
special cross-view rule). **Navigation** state — flamegraph zoom rectangle,
butterfly/inspect root, exact search string, histogram brush geometry — is
purely mechanical, stays **per-view**, and is *remembered per-view* so
switching away and back restores your place. Both serialize into the
shareable URL (selection + per-view nav state).

**"Everything" (B) is never literally 100%**:
The customer's canonical "selection B is everything" is **always a
cap-bounded, representative sample**, never every file — nothing in this system
is ever literally 100% of the data (the [[sampling-cap]] and [[order-key]]
guarantee a uniform spread, and the [[coverage]] badge tells the truth,
e.g. "100 / 45,000 files, 0.2%"). In the [[compare]] pop-out flow, "everything"
means A's **coarse scope minus the popped predicate** (the `NOT P` complement
within A's time/service window) — bounded by construction and the more useful
comparison (it controls for time-of-day, deploy version, etc.; comparing
against the literal all-time fleet would confound). A genuinely fleet-wide B is
an explicit, cap-bounded, clearly-labeled choice, not the default.

**Predicate / Consequent**:
Two roles the *same* set of fields plays. As a **predicate**, a field filters
(`service = Foo`, `region = Bar`, duration `> X`). As a **consequent**, the
same field (now a superset) is a computed result of the selection ("given
that predicate, p99 is now X", "mean poll duration is now X"). The diff
highlights how a consequent changed between A and B.

**View-local filter**:
A [[view]] may *add* filter criteria on top of the shared [[selection]]
predicate, expressed in that view's own terms — e.g. right-clicking a stack
frame in the flamegraph to keep only samples that passed through that code
path. This narrows within a view without redefining the cross-view
[[selection]]. A view-local filter can target **A only, B only, or both** —
all three are legitimate (both = keep the sides honestly comparable; one
side = ask a lopsided question). When a filter targets only one side, A and
B are no longer defined by the same predicate, and the UI must make that
asymmetry visible rather than silent. View-local filters are undoable via a
breadcrumb/zoom history.

**Significant difference**:
A ranked, surfaced change in a [[consequent]] between [[selection]]s A and B
— the "pull out the most significant differences" feature. Ranked
**per-[[view]]** in that view's own units (flamegraph: |Δ self-fraction| of a
frame, including frames present on only one side; span-stats: |Δ p99| /
|Δ mean| of a span type; tokio: |Δ mean poll| of a spawn location). A unified
**cross-view** ranking — one "what changed" summary drawing the globally most
significant deltas from every view — is the aspiration but is deferred: it
needs a common significance metric to compare heterogeneous deltas
(Δp99-of-a-span vs Δself-fraction-of-a-frame), which is unsolved. Ship
per-view first, learn what "significant" means, then attempt the summary.
_Avoid_: root cause (the customer's own hedge is "auto-(not-quite-)root-cause"
— we surface differences, we do not assert causation).

**Server-side diff**:
The diff and the ranked [[significant-difference]] list are computed
**server-side**, not in the browser. One endpoint (or a `compare=` mode on the
existing per-[[view]] endpoints) resolves both A and B [[selection]]s and
**schedules** fold work across both to keep their [[coverage]] *comparable*
(not rigid lockstep — the sides may drift within a tolerance), and streams
(1) the two sides already merged into a diff
structure and (2) the bounded ranked differences (by % and by absolute
sample-count, [[unsymbolized-frames-excluded]]). The **client** renders the
two synced panels + listing table and owns only interactive navigation
(zoom/search/inspect) over the shipped structure. Rationale: the server
already holds the data in the right shape; the design bounds output rather
than shipping every instance; per-node confidence intervals need server-side
per-file statistics; and one Rust diff+rank implementation over the
kind-agnostic span accumulator ([[span-kind-unification]]) serves every view,
versus re-implementing merge+rank per view in JS. See ADR-0004.
_Avoid_: client-side merge (the legacy `flamegraph_diff.js` two-stream merge
is superseded; at most a thin fallback).

**Coverage symmetry**:
The rule that A/B differences are only trustworthy when both [[selection]]s
have folded a *comparable* fraction of their [[matched-set]]. Comparing a
5%-folded A against a 90%-folded B produces an illusion — B appears to "have"
frames A is merely missing. The unified view drives both sides toward
comparable [[sampling-cap]]s and **suppresses/grays the
[[significant-difference]] ranking** until both sides clear a minimum
[[coverage]], showing an explicit "still folding — differences preliminary"
state instead. The [[server-side-diff]] endpoint schedules fold work across
both sides to keep coverage comparable, so gating rarely trips for long. This
is the v1 guard. The honest end-state is per-difference
**confidence intervals** (the planned per-node CIs that tighten as files fold,
already anticipated by [[coverage]]) — rank by statistical significance, not
raw magnitude, so a frame seen once in A and zero times in B sinks rather than
headlines. Raw-magnitude ranking with only a coverage badge is explicitly
rejected: an auto-surfaced "top difference" that is sampling noise destroys
trust on first use.

**Span kind unification**:
Polls (`PollStart`/`PollEnd`), worker parks (`WorkerUnpark`/`WorkerPark`), and
tracing spans are all analyzed as **spans**. The unification is at the level of
*in-code representation and analysis machinery*, not storage and not
presentation:

- **Storage stays separate.** The physical `polls/` projection and the
  `spans/` table remain distinct — polls are the highest-volume event class,
  and folding every poll into a full span row is a deferred storage cost (see
  the generalized-span design). The worker-active interval is a further span
  *kind*.
- **In-code representation is uniform.** After leaving Parquet, every kind
  deserializes into one span type. Kind-specific metadata (e.g. tokio spawn
  location) rides as typed fields — special-cased decoding
  (`derive(Deserialize)`-style), not a separate code path. `kind ∈ {tracing,
  tokio_poll, worker_park}`.
- **Analysis machinery is shared.** Anything computing span lengths,
  frequencies, percentiles, or composition runs over any kind. This is the
  point: if poll times for one spawn location are far longer than its peers,
  that is *interesting* and it is found by the *same code* that ranks tracing
  spans — a [[significant-difference]] across kinds.
- **Worker-active span** = the `[WorkerUnpark, WorkerPark)` interval during
  which a worker is **busy** (running tasks), *not* idle. Same polarity as a
  poll: more of them, or longer ones, means the runtime is churning through
  more work (busier). This is the interval CONTEXT's Worker attribution
  section calls the "active span" for CPU-time ratios.
  _Avoid_: park duration = idle time (the *span* is the busy interval between
  an unpark and the next park).
- **Presentation stays separate.** It does not make sense to list tracing
  spans and tokio tasks in the same [[view]] table. The [[view]] groups by
  kind; the shared machinery is what makes cross-kind observations possible.

**Group-by-attribute**:
A span type is *not* subdivided by its metadata: all polls are one type
(`kind = tokio_poll`), not "`tokio.poll` + spawn location" (this **departs**
from the generalized-span design §1.1, which baked spawn location into the
type UID). Instead, spawn location is typed metadata and the span machinery
has a first-class **group-by-attribute** capability — the tokio view is "spans
of kind `tokio_poll`, grouped by spawn location". This generalizes: tracing
spans can be grouped by any attribute too (e.g. `handle_request` grouped by
`route`). Grouping is a generic operation over metadata, not baked identity —
consistent with spawn location being just a `derive(Deserialize)` field on the
one uniform span type. The tokio view's on-cpu/off-cpu classification maps onto
the five-way composition; its top-N "longest polls" list is the span view's
slow-exemplars.
_Avoid_: spawn-location-as-span-type (grouping is a runtime operation, not
identity).

**Baseline (selection A)**:
The starting [[selection]], built in the viewer/S3 browser by quick-click
buttons ("whole service", "whole host" — new; do not exist today) or today's
drag-select. From the baseline you can **Inspect** (one-sided mode: today's
viewer — flamegraph looks like a flamegraph, span table like today's, no diff
ceremony) or **Compare**.
_Avoid_: selection A when discussing construction (it *is* [[selection]] A;
"baseline" names its role as the thing B is measured against).

**Compare (pop-out)**:
The action that creates [[selection]] B from the [[baseline]]. Any selection
criterion you could otherwise apply to narrow the current view is instead
"popped out as compare": rather than narrowing in place, it **splits** into
A vs B where **B gets the popped predicate `P`**. By **default** the two
sides are **disjoint** (no overlap): B = baseline ∩ `P`, and A gets a visible
`NOT P` filter chip auto-added so A = baseline ∖ `P` — "the matching subset vs
the rest", which maximizes the [[significant-difference]] signal (comparing a
tail against a population that *includes* it dilutes the contrast). The
`NOT P` chip is **not a hidden mode**: it is an ordinary filter on A, shown
like any other, and clicking it away reverts to "B vs the whole baseline"
(A ⊇ B). So B is normally a *refinement* of A's predicate, expressed entirely
in the one uniform filter model — no special "compare-against" toggle
machinery. The general filter-builder (arbitrary independent A and B) still
exists and is primary, but pop-out-refinement is the common path and the
[[capture-tray]]/derive-B presets are accelerators that populate it.

**Comparison permalink**:
The whole comparison is a **shareable, self-contained URL** — required to not
regress the UX audit's task-blocking S3 finding ("view state lives only in
memory"). Encoding: **A is encoded fully; B is encoded as a *delta* from A**
(the popped predicate(s) + any auto-added `NOT P`), because B is normally a
[[compare]] refinement of A. This keeps URLs compact as the arbitrary-filter
set grows and makes "B is a refinement of A" explicit in the URL structure.
A fully-independent B (rare) falls back to a full encode. Per-[[view]]
navigation state, the active view, the color encoding, and
[[derived-statistic-filter]] provenance ride as additional URL params. No
server-side saved-selection storage — the model stays URL-self-contained
(creds in sessionStorage, everything else in the URL).

**Capture tray**:
The existing "Add to diff" A/B accelerator (today in `flamegraph_diff.js`):
analyze a scope, click "Add to diff" → fills A, then B. In the unified view it
is demoted to one of several accelerators that *populate* the [[selection]]
filter-builder (alongside the derive-B presets: same-scope-different-host,
time-shifted), not a separate diff-construction mechanism.

**Migration path**:
The unified view is grown by **forking/copying the current span explorer
page** as the host shell, then *adding* to it — not a greenfield rewrite and
not flamegraph-first. The span explorer is the surface already closest to the
destination: it hits `/api/span-stats`, already has the catalog →
select-span-type → drill-into-flamegraph flow, already computes the
percentiles/composition/exemplars that are the raw material for
[[significant-difference]], and already owns the span→flamegraph deep-link.
Forking protects the live page while building. Growth order:
1. Add the A/B [[selection]] model + [[compare]] pop-out to the fork.
2. Diff the span catalog it already renders (its [[significant-difference]]
   instance).
3. Embed the **rich flamegraph renderer as a [[view]] inside the shell** (the
   [[renderer-direction]] two-synced-panels work) — the flamegraph stops being
   a separate `window.open` page you deep-link out to and becomes a view
   *inside* the unified shell.
The three existing pages (flamegraph, span_explorer, tokio_stats) stay alive
and **un-deprecated** until the unified view reaches per-[[view]] feature
parity; a page is deleted only when its unified equivalent is strictly better.
Span/tokio diff, [[group-by-attribute]], and the cross-view
[[significant-difference]] summary are deferred beyond the first slice.

## Worker attribution

A **worker** is a tokio runtime thread with a stable `WorkerId`. A worker
runs on an OS thread, identified by its `tid`.

The `tid → worker_id` mapping is **not constant**. During
`tokio::task::block_in_place`, a worker's OS thread temporarily acts as a
blocking-pool thread (and tokio races a blocking-pool thread to take
over the worker's scheduler responsibilities on a fresh OS thread). The
mapping must therefore be reconstructed per time interval, not assumed
fixed for the trace. The same tid can also leave and later re-acquire a
worker role: it joins the blocking pool when displaced, and any
blocking-pool thread (including the original) may race to take over a
worker's core after a future `block_in_place`.

- **Authoritative source:** `WorkerPark.tid` / `WorkerUnpark.tid`. These
  bracket the intervals during which a given `tid` is acting as a given
  `worker_id`.
- **Unreliable hint:** `CpuSample.worker_id` on the wire. The producer
  fills it from a `tid → worker_id` table updated only on
  `on_thread_start` / `on_thread_stop`, which `block_in_place` does not
  fire. Analysis tooling MUST ignore this field and re-derive worker
  attribution from park/unpark events. The field will be removed from the
  trace format in a future change.

## Block-in-place gap

A **block-in-place gap** is the interval during which a worker `W`'s
binding to an OS thread is unknowable from the trace alone. It is
detected post-hoc from a `WorkerPark { worker_id: W, tid: B }` for which
the most recent prior park/unpark on worker `W` was on a different tid
`A`.

The gap is the interval `[last_event_on_W_at_A, park_at_B)`. Within the
gap, the analysis layer treats samples on tids `A` and `B` as
**unattributable to a worker** — the handoff happened at an unknown
point in the interval, so neither tid can be confidently labeled.
Samples are still visible by tid, but worker-derived views (e.g.,
flamegraphs grouped by worker) must drop them.

The viewer surfaces the gap to the user as a `block_in_place` annotation
on worker `W`'s timeline. Detailed investigation (which tid was running
which code at which moment) is left to the agent analysis toolkit, which
can correlate by tid against CPU samples, sched events, and stack
contents.

When a gap is detected on worker `W`, any **active span** (the
unpark→park interval from which CPU-time ratios are computed) that
crosses the gap is **discarded**, not split. The CPU-time delta across
the gap mixes two different threads' `CLOCK_THREAD_CPUTIME_ID` readings
and is therefore meaningless; dropping the polluted span is more honest
than reporting a contaminated ratio.
