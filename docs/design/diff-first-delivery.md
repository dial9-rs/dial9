# Diff-first delivery plan — vertical slices

Companion to [diff-first-unified-view.md](./diff-first-unified-view.md) (the
PRD). This is the sequencing: how we get from today (three separate pages, a
client-only flamegraph diff) to the diff-first unified view.

Each slice below is stated as a **vertical slice** — it cuts through data →
API → UI and ends at a **demoable** capability — except **S0**, which is
foundational infra the user explicitly chose to build first (see
[§ Fold scheduler](#s0--fold-scheduler-foundational)).

## Principles

- **Demoable end state per slice.** Every slice (S1+) ends with something a
  user can drive in the viewer, not an internal-only change.
- **No regression to the three existing pages** until the unified view reaches
  per-view parity. They stay live and un-deprecated.
- **`b = ∅` is the compatibility contract.** Every API change keeps the
  single-selection response byte-for-byte unchanged when `b` is absent (PRD
  §8.2), so existing callers never break.
- **Reuse before invention.** Compare composes the existing `Scope`,
  `resolve()`, `fold_stream`, `Coverage`, and per-view response types (PRD
  §8.1). New code is confined to the scheduler (S0), the merge+rank (S1/S4),
  and the UI shell.

---

## S0 — Fold scheduler (foundational)

**Why first.** Today every request independently calls `resolve()` +
`fold_stream::drive`. Two overlapping folds (two tabs, two users, or the two
sides of a diff) duplicate S3 GETs and Parquet decodes, and nothing balances
their progress. A **general fold scheduler that knows all currently-desired
folds** dedupes in-flight work, caps total concurrent S3/decode, and — as a
side effect — makes coverage-symmetry balancing a scheduling policy rather than
bespoke diff code. The user chose to build this first so coverage symmetry is
correct from the *very first* diff and dedup benefits land immediately,
accepting that S0 has no standalone user-visible capability. ADR-0003's
idempotent, listing-as-folded-set model already makes folds cacheable, so the
seam is clean.

**Scope.**
- A process-wide scheduler owning fold execution: callers submit **fold
  intents** (a set of source keys to fold into a given output context) and
  receive a stream of completions.
- **Dedup:** an in-flight fold of a given `(source_key → output)` is shared by
  all callers that want it, rather than re-fetched/re-decoded.
- **Concurrency cap:** one global limit on concurrent S3 GET + decode, replacing
  the current per-request behavior.
- **Fairness / balancing policy:** the hook coverage symmetry will later use to
  keep two (or N) related work-lists progressing at comparable rates.
- **`fold_stream::drive` is refactored to consume the scheduler** instead of
  folding files itself; the two `Phase`s (Seeding, Folding) and the
  snapshot-per-step cadence are preserved.

**Data / API / UI.**
- Data: no schema change. Pure execution-path infra.
- API: **no visible change.** All three endpoints route through the scheduler
  and must return identical responses and SSE cadence.
- UI: none.

**Demo / exit criteria.** Not a user demo. Exit = the three existing endpoints
pass their current tests unchanged through the scheduler, plus new tests for
dedup (two overlapping requests fold each shared file once) and the global
concurrency cap. Run the stress suite — this touches the hot fold path.

**Risk.** Highest-risk slice (reworks the shared fold engine with no new
feature to justify it if it slips). Mitigated by the `b = ∅` / no-visible-change
contract: it is a pure refactor gated on the existing endpoints' test suites.

---

## S1 — Load-test window comparison (spans p99), end-to-end

**The demo.** Compare the **beginning of a load test vs the end**: same service
and hosts, two different time windows (`start_ns`/`end_ns`). The spans view
shows one aligned table ranked by p99 shift — "as load ramped, `db.query` p99
went 12ms → 340ms." This is the first true diff and a real diagnostic.

**Why this is the ideal S1.** Both selections differ **only in scope**
(`start_ns`/`end_ns`) — no new filter plumbing, no pop-out UX yet, no derived
statistics. It exercises S0 with two overlapping-but-distinct file sets
(the scheduler folds the union, balances the two windows) and proves the whole
data→API→UI path on the simplest possible B.

**Data.** None new — `spans/` already carries `end_ns - start_ns` and
percentiles are already computed.

**API.** Add the `b=<base64url(SpanStatsParams)>` **compare mode** to
`/api/span-stats` (PRD §8.2):
- `b` absent → today's `SpanStatsResponse`, unchanged.
- `b` present → resolve both selections (two `Scope`s differing only in the time
  window), submit **both** fold intents to the S0 scheduler, and stream a
  `DiffSnapshot<SpanStatsResponse>` (PRD §8.4): `{ a, b, diff, ranked,
  coverage_a, coverage_b, comparable }`.
- `ranked`: `Difference` rows for span types, sorted by |Δ p99|, with
  `membership` (Both / OnlyA / OnlyB). Bounded top-N.
- `comparable`: the coverage-symmetry gate, driven by the scheduler's balanced
  progress (S0).

**UI.** Fork `span_explorer.html` → the unified shell (first appearance). The
shell has three regions: a **collapsible time-navigation pane** (top or side),
the aligned diff table, and the significant-difference rail.

- **Collapsible time pane = the embedded S3-browser heatmap.** This is how you
  pick A and B and how you stay oriented in time. It is the existing
  time × (service/host) **density heatmap** (X = wall-clock, rows = hosts, cells
  = bytes/time) — *not* a new widget. **Drag window A, drag window B**; each
  drag produces the `{keys, t0, t1}` selection the browser already computes,
  which `encodeAggregationParams()` already turns into `start_ns`/`end_ns`.
  A and B render as two labeled brushed bands (teal / coral) on the same axis.
  The pane collapses so the table can take the full height once the windows
  are set.
  - **Prerequisite inside this slice:** extract the ~350 lines of heatmap
    renderer + interaction currently inlined in `index.html`
    (`renderHeatmap` / `drawHeatmapCanvas` / `drawHeatmapAxis` /
    `setupHeatmapInteraction`, hardcoded to element IDs and module-globals) into
    a **container-scoped component**. The pure `heatmap.js` core (density,
    grouping, ticks, gaps, boot transitions) is reused **unchanged**. `index.html`
    then consumes the same component, so the browser and the unified pane do not
    diverge.
- **Exemplar overlay on the time axis (the second reason for the pane).** Each
  span type's `Exemplar` rows already carry `start_ns` + `host` (epoch ns; no
  backend change). Overlay them as markers on the heatmap: `start_ns / 1e9` →
  the existing `timeToX()`, row matched by `host`. This is an additive canvas
  pass (same shape as the existing boot-transition dividers). Clicking a marker
  deep-links to that exact span (`focus_start`/`focus_end`), and — because the
  markers sit on the A/B bands — you *see* that the slow exemplars cluster in
  window B, not A. Selecting a span-type row highlights its exemplars in the
  pane.
- One **aligned diff table**: one row per span type, the **p99 distribution
  overlay** column (A/B histograms + p50/p99/p999 markers), occurrences A→B.
- The **significant-difference rail**, ranked by p99 shift, derived from
  `ranked`.
- Per-side **coverage badges** and the **"differences preliminary"** gate while
  `comparable` is false.

**Demo / exit criteria.** On a real load-test trace: open the fork; in the time
pane, drag window A over the first minutes and window B over the last minutes;
see the aligned table + ranked rail flip from "preliminary" to live as the
scheduler balances coverage; see the slow `db.query` exemplars light up under
window B on the time axis. Collapse the pane to read the table full-height. The
three existing pages are untouched.

---

## S2 — Compare / pop-out UX

**The demo.** From a single baseline (Inspect), click **Compare → pop out
> p99** (or "pop out this span type") and watch it split into A vs B with the
`NOT P` complement auto-added to A.

**Builds on S1's endpoint — no new server capability.**

**API.** None (S1's `b=` mode already accepts arbitrary B params). Possibly a
tiny helper to compute the frozen p99 threshold, but that can be client-side off
the current snapshot.

**UI.**
- **Pop-out**: any narrowing criterion splits into A vs B; B = baseline ∩ P;
  A gets a visible, removable `NOT P` chip (PRD §2.3).
- **Derived-statistic freezing**: "> p99" freezes to an editable
  `duration_ns > X` chip with provenance ("derived: p99 @ cov …") and a refresh
  (PRD §2.4).
- **Inspect ⇄ Compare** layout transition (single full-width ↔ split).
- **Shareable-URL codec**: A encoded fully, B as a delta from A (PRD §9).

**Demo / exit criteria.** Pop out ">p99" from a baseline; copy the comparison
link; reload it and land on the same diff. Remove the `NOT P` chip → B vs whole
baseline.

---

## S3 — Full spans diff vocabulary

**The demo.** The rich spans diff: composition **reallocation** ("on-cpu −37pp,
async-wait +24pp"), inline segment times, new/gone badges — the whole aligned
table from the mock.

**API.** Minor — the `DiffSnapshot` `diff`/`ranked` already carry composition
and membership; this slice surfaces per-category composition deltas in the
envelope if not already present.

**UI.** The composition cell (A-over-B stacked bars, inline-labeled with time),
new/gone row badges, rank-by-share vs rank-by-Δ% toggle. Completes the spans
view to the mock's fidelity.

**Demo / exit criteria.** The spans diff matches the
`concept-diff-first.html` mock.

---

## S4 — Flamegraph view diff (server-side merge)

**The demo.** Two synced rich flamegraph panels *inside* the unified shell (no
`window.open`), delta-colored, with the flamegraph significant-difference
listing table.

**API.** Add the `b=` compare mode to `/api/flamegraph`, streaming
`DiffSnapshot<FlamegraphResponse>`. The tree **merge moves from
`flamegraph_diff.js` into Rust** (PRD §8.3), over the kind-agnostic accumulator.
Both sides fold via the S0 scheduler.

**UI.** Embed the feature-rich flamegraph renderer as a **view inside the
shell**, two synced panels (zoom/search/inspect mirrored), user-selectable
coloring, the differences listing table. The legacy `?diff=1` client merge is
kept only as a fallback for the old page until retired.

**Demo / exit criteria.** Compare two windows on the flamegraph view; zoom in
one, the other follows; unsymbolized frames excluded from the ranking.

---

## S5 — Tokio view via group-by-attribute

**The demo.** Tokio poll diff in the unified shell: `kind = tokio_poll` grouped
by spawn location, poll-duration distribution + on/off-cpu reallocation per
spawn location, A vs B.

**API.** Add `group_by=<attribute-key>` to `/api/span-stats` (PRD §8.5,
query-side of ADR-0005). `group_by=spawn_location` over `tokio_poll` spans is
the tokio view. Retain the `polls/` projection until measured evidence supports
collapsing it into `spans/`.

**UI.** The tokio view as a grouping preset of the spans view; the group-by
control exposes other attributes (route, host, …). The old `/api/tokio-stats`
page stays live until parity.

**Demo / exit criteria.** Load-test window diff, tokio view: see which spawn
location's polls got slower and how their on/off-cpu split reallocated.

---

## Deferred beyond these slices

- **Cross-view significant-difference summary** (one ranked "what changed"
  across all views) — needs a common significance metric (PRD §6).
- **Per-difference confidence intervals** — the honest end-state for ranking
  (PRD §7.2).
- **Per-instance correlation** ("elapsed correlates with `db_time`
  span-by-span") — needs per-span joins (PRD §3.1).
- **Collapsing `polls/` into `spans/`** — storage decision gated on measurement.
- **Retiring the three legacy pages** — only once the unified view is strictly
  better per view.

## Dependency order

```
S0 (scheduler) ──► S1 (spans window diff) ──► S2 (pop-out UX) ──► S3 (spans vocabulary)
                        │
                        ├──► S4 (flamegraph diff)      [needs S0 + the shell from S1]
                        └──► S5 (tokio via group-by)   [needs S1's spans endpoint]
```

S1 establishes the shell and the `DiffSnapshot` contract; S2–S5 extend it. S4
and S5 depend on S1's shell but not on each other.
