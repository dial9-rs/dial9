# Migration tickets - chunk 2 (viewer page + rollout)

Continues `chunk-1-foundation.md` (same conventions, same doc-index; read its
header first). This chunk migrates `viewer.html` by slice, lands the chunk-2
UX amendments, and finishes the staged rollout.

**Layout decision (maintainer, 2026-07-07):** unified time-aligned track
column (concept 1) + persistent inspector sidebar (concept 1) + issues rail
replacing the "0/74 Next" stepper (concept 2) + minimap + status bar (all
concepts). Mocks: `docs/ui-inventory/mocks/concept-{1,2}.html`.

Ownership summary (features/02 sections -> tickets):
A,T,U,W -> T21 · B -> T34 · C,D,E -> T33 · F -> T25 · G -> T22 · H -> T23 ·
I,V -> T24 · J -> T26 · K -> T27 · L -> T28 · M -> T29 · N -> T30 ·
O -> T36 · P,Q,R -> T31 · S -> T32.
(Section letters refer to the row tables in features/02-viewer-html.md;
where a row's mechanism ships in one ticket and its surface in another, the
ticket text names the seam explicitly.)
UX amendments (04-ux-findings): mapped per ticket below; every amendment PR
updates the owned inventory rows in the same diff (contract-amendment rule,
established in T15).

---

## T21 - Viewer shell, layout, and app chrome

**Goal:** The new viewer page skeleton per the layout decision: toolbar row,
minimap slot, unified track column, inspector slot, status-bar slot; plus
app-wide chrome (toasts, help overlay integration, hint chips, empty state).

**Context:** mocks `concept-1.html` (structure to replicate);
`02-architecture.md` 2.1/2.4; features/02 sections A (shell/global), T (help),
U (toasts), W (cross-cutting rows except those in T23); UX amendments riding
here: S1 (analysis surfaces visible by default - the folds die with the
unified column), F4 (empty state teaches next steps), F5 (hint chips
persistent), F6 (contrast fixes in styles), K6 (tab order follows task flow).

**Owns:** features/02 sections A (A1-A18, incl. A16 ARIA live region - T23
consumes the announce mechanism, T21 lands it), T, U, and W (all W rows;
they are cross-cutting utilities, most implemented by chunk-1 lib tickets -
T21's obligation is that each W behavior is reachable in the new shell).
NOT owned here: TZ/format toggle BUTTONS (section E -> T33), esc-ordering
row (section D -> T33); T21 provides the esc-cascade mechanism they hook.
Amendments: S1, F4, F5, F6, K6. Concrete templating dependency: `lit-html`
(the library itself; ADR 2.4's class-of-library made concrete - S1-justified
in the PR).

**Work:** `src/pages/viewer/` entry + layout components; slots consume later
tickets' components; ARIA landmarks + labeled controls (04 F1 - the axe
critical fixes) built in from the start, enforced by T12's axe gate.

**DoD:** check: shell renders on the demo trace with every track slot
showing its label + a correctly-sized empty canvas (layout widths from
`lib/canvas/layout`) - that is the "placeholder" definition; check: owned
A/T/U rows green in the T12 row-walker (help overlay, toasts, ARIA
announcements); check: axe clean; check: tab-order dump matches task flow
(toolbar -> minimap -> tracks -> inspector).

**Out of scope:** every track/panel's content (T22-T32).

**Deps:** chunk 1 complete (T02-T09, T12, T16-T18 for data; T19/T20 integrate
here).

---

## T22 - Worker lanes track

**Goal:** The lanes canvas component - the viewer's core visualization -
ported to `components/canvas/lanes` under the new render rules.

**Context:** features/02 section G (G1-G20: lane grid, background states, CPU
tint, poll bars, selected-task/span/waker highlights, CPU ticks, sched
triangles, wake markers, local-queue step line, click semantics, hover
tooltip content, scroll sync, legend); `03-performance-findings.md` F1 - the
local-queue step line and dashed markers MUST use the T08 stroke batcher
(per-column vertices, one path per style); G20 is DEAD (drop with ledger
entry). Amendments: F3 partial (legend covers q:NN and all in-lane marks),
K8 (click-target affordance: cursor/hover feedback where clicks resolve).

**Owns:** features/02 G1-G19 (G20 retired via ledger). Amendments: F3(lanes),
K8.

**Work:** `render(ctx, state, layout)` pure function; per-lane redraw driven
by the store's renderer registry (selection change redraws lanes only -
03 F2's fix); pixel-LOD + coalescing via T08; hover data assembled through
`lib/trace/query` (no O(allSpans) scans - 03 F6).

**DoD:** check: row-walker green on G rows (G20 ledger-retired); check:
behavioral differ - identical poll/task selection results vs legacy on
J2/J4; check: perf on the x8 pan-storm fixture, same machine, new vs
legacy: (a) renders coalesced to <= 1 per frame (N2 as written), (b) total
lanes-attributed script time <= 10% of the legacy baseline run, (c) stroke()
self-time <= 10% of total lanes render time (legacy: 76%) - record all three
numbers in the PR; check: Vitest for downsample/coalesce usage.

**Deps:** T21, T08, T09.

---

## T23 - Pointer + keyboard interaction state machine

**Goal:** All lane-area input as `lib/interact` state machines dispatching
store actions (architecture 2.5): pan/zoom/wheel, Shift+drag region,
Alt+drag zoom, click-select, keyboard region selection.

**Context:** features/02 section H (H1-H13; the keyboard-selection state
machine is row H9). ARIA announcements are A16 (T21 lands the live region;
T23 dispatches announcements through it). 03 F2/F6 (handlers never render,
never scan); T20's router + actions are the substrate. Seam on H7
(Shift+drag): T23 owns the gesture -> store action; WHAT the region opens
(flamegraph/blocking/heap by data present) is T32's.

**Owns:** features/02 H1-H13 (no W rows - section W has no keyboard rows).
Amendments: viewer INTEGRATION of K4 (zoom history on `z`) and K5 (WASD on
the timeline) - mechanisms are T20's.

**Work:** pointer machine (thresholds preserved: 3px drag intent, 100ns min);
wheel: plain=scroll lanes, Ctrl/Cmd=zoom-at-cursor (H5); keyboard selection
per H9 unchanged; zoom stack in store; ARIA announcements preserved.

**DoD:** check: row-walker green on H rows; check: behavioral differ vs
legacy on J2, J3, J4 (the pan/zoom/select-exercising journeys); check:
Vitest on the state machines (every transition incl. cancel paths); check:
wheel-zoom coalesced to <= 1 render/frame (03 F2 regression test via T12
perf probe).

**Deps:** T21, T22, T07, T20.

---

## T24 - Crosshair overlay, hover tooltip, info readout

**Goal:** The transient channel: crosshair canvas + tooltip + at-cursor
readout, zero DOM writes on the overlay path, no layout read-after-write
(03 F3's fix).

**Context:** features/02 sections I (I1-I6) and V (tooltip system); 03 F3
(measured: 171 forced layouts/183 mousemoves - the anti-pattern to kill).
Seam with T22 on the lane tooltip (G16): T24 owns the tooltip COMPONENT
(placement, render template, transient-channel updates); T22 supplies the
per-poll hover DATA via `lib/trace/query` calls. Inspector mirroring per
04 S4 ("at-moment stats in one surface"); tooltip stays for hover
ergonomics.

**Owns:** features/02 I1-I6, V rows. Amendments: S4 partial (at-cursor
stats mirrored into one persistent surface). T24 CREATES the `transient`
store slice (shape declared in `types/state.d.ts`, T06) and DEFINES the
at-cursor readout store contract that T31's inspector renders - document
both in the types.

**Work:** overlay canvas on its own RAF channel (`transient` slice);
tooltip as declarative component with cached-dimension placement (no
offsetWidth-after-innerHTML); at-cursor readout written to the store
contract (T31 renders it; until T31 lands, a minimal readout stub in the
inspector slot proves the wiring).

**DoD:** check: row-walker green on I/V rows; check: T12 perf probe - hover
storm produces ZERO forced-layout entries from the tooltip path (03 F3
regression); check: crosshair alignment vs lanes asserted by Vitest on the
shared layout math + one T12 visual check (the T25 method; "N4" extends the
axis-alignment invariant to the overlay); check: at-cursor store contract
populated during hover (Vitest on the store, plus the stub renders it).

**Deps:** T21, T22, T07.

---

## T25 - Timeline header / time axis track

**Goal:** The time axis: ticks, TZ/format modes, alignment invariant.

**Context:** features/02 section F (F1-F...); the shared time-axis
alignment invariant (02-architecture N4's A13 clause, via T08 layout);
time-legibility friction family (04 S2 / issue #137). TZ/format state:
consumed from the store (`uiPrefs.timeMode`/`uiPrefs.tz`, exposed by T21's
shell state - the axis reads the slice, never a toggle button directly;
buttons are T33's). Amendment rule, concrete: when the visible span's start
and end fall on different calendar days (evaluated in the CURRENT tz mode),
tick labels gain a date prefix (`MM-DD HH:MM:SS`); same rule as T15's
heatmap-axis amendment.

**Owns:** features/02 F rows. Amendment: viewer-axis date qualification
(new ledger row - no 04 finding ID exists for it; justified by the S2/#137
family).

**DoD:** row-walker green; alignment asserted pixel-exact against lanes/panel
tracks at three widths (Vitest on layout math + one visual check in T12).

**Deps:** T21, T08.

---

## T26 - Spans track

**Goal:** Span panel as a time-aligned track: clusters, filter, legend chips,
percentile filter, span focus chain.

**Context:** features/02 section J (J1-...: rendering, filtering, chip
toggles, focus/ancestor chain semantics); 03 F5 (selectSpanRenderSet +
computeSpanLayout re-derivation -> derived-cache via store, binary-search
visibility windows); chip/legend rebuild churn (03 F7) -> declarative keyed
templates. Layout decision applied: the focused-span metadata surface
(J3 label area, J4 copy buttons) STAYS with the spans track (the unified
column keeps per-track label areas per the concept-1 mock); the inspector
(T31) is for selection detail, not span-track metadata. Selection-driven
dimming of this track (04 S4) is implemented HERE, reacting to the
`selection` slice - T31 only drives the slice.

**Owns:** features/02 J rows. Amendments: keystroke filtering coalesced
(03 F2: no renderAll per keypress - store-debounced); S4 partial (spans
track dims to selection).

**DoD:** check: row-walker green on J; check: filter typing produces <= 1
render/frame (T12 perf probe); check: span focus behavioral-diffed vs
legacy on J4; check: selection change dims non-selected spans (Vitest on
the render input + one T12 visual); check: derived cache invalidation
Vitest.

**Deps:** T21, T07, T08, T09.

---

## T27 - Custom events track

**Goal:** Custom-events panel as a track: markers, legend, click-to-pin,
hover guide line.

**Context:** features/02 section K; visibility scan bounded (03 F5 - the
custom-events O(N) scan - fixed via 03 F6's binary-search-helpers rule).
Seams: the hover guide line and pinned-event marker are I4/I5 (T24 renders
them on the overlay); T27 DISPATCHES the store state (`transient.hoverEvent`,
`selection.pinnedEvent`) that T24 consumes. The event-KV sidebar content is
T31's surface; T27 dispatches the selection, T31 renders it.

**Owns:** features/02 K rows (dispatch side of I4/I5 interplay). S4
partial: events track dims to selection (same rule as T26).

**DoD:** check: row-walker green on K; check: click-to-pin dispatches the
selection slice (Vitest) and, once T31 lands, the sidebar KV
behavioral-diffs vs legacy (mark this item deferred-until-T31 in the PR if
T31 has not landed); check: bounded-scan Vitest; check: dim-on-selection as
T26.

**Deps:** T21, T07, T09, T24 (overlay contract).

---

## T28 - CPU usage track

**Goal:** Process CPU panel as a track.

**Context:** features/02 section L. Rendering reality: L is a fillRect bar
chart (avg-cores-over-time), not a curve; its stroked primitives are the
dashed capacity line and grid lines - those use T08's batched-stroke path
(03 F1's rule), the bars use the coalescer. Track hosting/height contract
comes from T21's shell (the old 92px/24px fold heights are superseded by
track layout; section O semantics land in T36).

**Owns:** features/02 L rows.

**DoD:** check: row-walker green on L; check: rendered values
behavioral-diffed (avg/max readouts exact vs legacy).

**Deps:** T21, T08.

---

## T29 - Queue depth track (+ the queue legibility amendment)

**Goal:** Queue panel as a track AND the S6 fix: global+local queue in one
labeled, always-visible representation.

**Context:** features/02 section M - ALL its rows, which go beyond the
global/local series: M3 active-task line (right y-axis), M6 hover info
(routed through T24's at-cursor store contract, not a corner div), M7
drag-select-spawned-tasks -> task list, M8 task-id link clicks (list +
links render in T31's inspector; T29 dispatches the selection). 04 S6
(split-brain; #282: global invisible at 0; note M2's max-local line already
exists in-panel - S6's fix is unifying the ENCODINGS and labeling, not
inventing a local series); F3 queue-legend coverage.

**Owns:** features/02 M rows. Amendments: S6, F3(queue).

**Work:** track shows global + max-local + active-task series with explicit
zero baseline (0-valued global renders as a visible flat line); legend
entries match in-track encodings; in-lane "q:NN" gains a legend entry
(coordinated with T22); M7/M8 dispatch through the selection slice.

**DoD:** check: row-walker green on ALL M rows (M7/M8 items marked
deferred-until-T31 if it has not landed); check: J7 behavioral-diff - same
numbers, presentation change ledgered; check: zero-global renders a visible
baseline (Vitest on the scale function + one T12 visual).

**Deps:** T21, T08, T22 (legend coordination), T24 (at-cursor contract).

---

## T30 - Task detail track

**Goal:** Per-task detail (polls, wakes, lifetime). Hybrid split, decided:
the task-detail TIMELINE (per-task polls/wakes over time) is a track owned
here; the TEXTUAL detail (task id, spawn location, counts) renders in
T31's inspector Task tab from the same derived data.

**Context:** features/02 section N; 03 F5 (its renderTaskDetail case:
per-frame re-collect+sort of all polls -> derived cache keyed by the
selection slice). Waker-hover highlight: T30 dispatches
`transient.hoveredWakerTaskId` (T24's slice); T22's lanes consume it
(features/02 G8) - the store field IS the contract.

**Owns:** features/02 N rows (track + derivation; T31 renders the textual
tab).

**DoD:** check: row-walker green on N; check: task numbers exact vs legacy
(behavioral differ, J4); check: derived-cache Vitest (selection change
invalidates, pan does not); check: waker-hover dispatch Vitest.

**Deps:** T21, T07, T09, T22, T24 (transient slice).

---

## T31 - Inspector sidebar (tabs: poll/event/related/stack)

**Goal:** The persistent inspector (concept 1): today's stack-sidebar
content + selection re-scoping (04 S4's fix).

**Context:** features/02 sections P, Q, R (sidebar mechanics, event/related
detail, poll detail + blocking/sched panel); 04 S4 (selection re-scopes
detail surfaces; at-moment stats in ONE place - receives T24's readout);
history: sidebar drift #291/#304/#305 (why persistent-sidebar is the settled
direction).

**Owns:** features/02 P, Q, R rows. Amendments: S4, F7 (selection status +
explicit clear affordance).

**Work:** `mount(el, store)` component; tabs preserved (+ Task tab
rendering T30's derived data); selection slice drives content; resizable
(P rows); "what is selected" line + Esc/close affordance. DEAD rows P8
(width persistence declared but unimplemented) and R6 (handler-less
summary-row) follow the G20 pattern: implement P8 properly (width ->
uiPrefs; it is trivially in scope of "resizable"), retire R6 via ledger.

**DoD:** check: row-walker green on P/Q/R (P8 now VERIFIED, R6
ledger-retired); check: click-poll-with-samples opens Poll Detail exactly
as legacy (behavioral diff); check: S4 acceptance limited to T31's surface
- selecting a task re-scopes the INSPECTOR in the same action (the track
dimming halves of S4 are T26/T27's DoDs; together they complete S4, T37
verifies the union).

**Deps:** T21, T07, T24, T30 (Task-tab data).

---

## T32 - In-viewer flamegraph + region analyses

**Goal:** Shift+drag region -> flamegraph / blocking-calls / heap in the
sidebar, plus the S7 amendment: frames link back to timeline time.

**Context:** features/02 section S (NOTE: that section's rows are labeled
F1-F19 inside the S section - an inventory numbering quirk; "S rows" here
means those); CPU/heap/idle/task-dump modes, region scoping, sidebar tabs.
04 S7: contradictory counts - first work item is root-causing the "8993"
toolbar count vs "147 samples" page count on the same trace (no doc
explains it; suspected different units - samples vs region-filtered - the
investigation result decides "explain" vs "fix"). K7 (frame keyboard
traversal): implement in the PAGE layer - walk the flamegraph's data tree
(built by `lib/trace/analysis`) and drive zoom through whatever public
surface `flamegraph.js` exposes; if none exists, K7 defers with a ledger
note + a T47 line item (core API addition) - the frozen core is NOT edited
here.

**Owns:** features/02 section-S rows. Amendments: S7 (frame->time link +
count reconciliation), K7 (embedded, best-effort per above).

**DoD:** check: row-walker green on section-S rows; check: region-select
flows behavioral-diffed (J2/J5); check: count root-cause documented in the
PR + reconciliation landed or ledgered; check: frame click offers "show in
timeline" - viewport navigates to the [min,max] timestamp extent of that
frame's samples within the analyzed region (new behavior: ledger + new
inventory row in-diff); check: partial-window badge shown when the region
exceeds the resident window (2.8).

**Deps:** T21, T23, T31, T17.

---

## T33 - Toolbar, file info, and the issues rail

**Goal:** Toolbar (file info, analysis buttons, time modes) + concept 2's
issues rail replacing the POI stepper.

**Context:** features/02 sections C, D, E (toolbar file info, POI cluster,
analysis buttons, Set Range, time display); mock `concept-2.html` (rail
structure); 04 S5 (blind stepper -> ranked list with timeline markers), F2
(jargon tooltips on every toolbar control), 04 S2 partial (Set Range gains
T20's `g` goto-time integration); POI data = the existing detectors in
features/02 section C rows (C2 `filterPointsOfInterest`, C3 worst-first, C7
jump semantics - trace_analysis.js). Rail v1 scope: existing POI detectors
ONLY; the mock's "red-flags summary" chip renders the same detectors'
COUNTS, no new detector source is built here. Both the rail and T35's
minimap ticks consume detector output via `lib/trace/analysis` (T09) - no
T33<->T35 dependency.

**Owns:** features/02 C, D, E rows. Amendments: S5, F2.

**Work:** toolbar components; issues rail: sortable list (worker/kind/time/
duration), `n`/`p` steps it (T20 binding), row click centers viewport +
selects; POI tick marks on the minimap (T35 coordination); "Parse perf"
demoted to help/info menu (F2).

**DoD:** check: row-walker green on C/D/E (amended rows in-diff); check:
every toolbar control has a tooltip (axe + census); check: rail count
parity - with the demo trace, default POI filter, worst-first on, the rail
lists exactly the count the legacy "x/74" stepper reports (behavioral diff
on detector numbers).

**Deps:** T21, T07, T09; T20 (bindings). (T35 renders minimap ticks
independently; no dep either way.)

---

## T34 - Load chrome: drop zone, file loading, New File

**Goal:** Load/drop-zone surfaces + the D2 escape-hatch fix (confirm before
discarding a loaded trace; dismissible load section).

**Context:** features/02 section B (drop zone, picker, drag-drop, URL
loading, streaming progress UX - the load-timing line is a keep-exactly
feature). The amendment sources, precisely: 04 finding S3's "New File
discards without confirmation, no recovery in either direction" clause, and
GitHub issue #281 whose ask (from the friction mining) is: the load-new-file
section cannot be closed once opened; wants Esc + a close button. T16/T17
supply the pipeline; progress surfaces per the 2.8 feedback hard edge
(status bar - T35).

**Owns:** features/02 B rows. Amendments: the S3 new-file-confirm clause +
#281 dismissal (ledger; S3's URL-state clause is T19's, its status-line
clause T35's - noted so S3's ownership is complete across tickets).

**DoD:** check: row-walker green on B; check: drop/pick/URL loads
behavioral-diffed; check: New File confirms when a trace is loaded (amended
row in-diff); check: the load section closes via Esc AND a visible close
control, and reopening works (the two #281 asks).

**Deps:** T21, T16, T17.

---

## T35 - Minimap + status bar

**Goal:** The two new persistent surfaces every concept shares: overview
minimap (tier-1 data, POI ticks, draggable viewport) and status bar
(selection state, view range, copy-link, key hints).

**Context:** 04 S8 (no position context) + S3 (status/copy-link clause) +
F7 (selection visibility); mocks (minimap + status bar in all concepts);
data: segment listing extents come from T17's segments slice (2.8 tier 1),
aggregate density from T18's client with its coverage signal (fall back to
listing-metadata density when `partial`/`none`); T19 codec supplies
copy-link; POI ticks from `lib/trace/analysis` (T09 - same source as T33's
rail); segment fetch/parse progress (2.8 feedback hard edge) renders here.

**Owns:** new inventory rows (added in-diff, ledger "additions" section).
Amendments: S8, S3(surface), F7(status), 2.8 progress feedback.

**Work:** minimap canvas (density from tier-1 sources, viewport box drag =
store viewport update, POI ticks from T33's detector data); status bar
component (selection line, view range, copy-link button, progress segments,
key hints).

**DoD:** check: minimap navigates a multi-segment trace whose tail is
UNFETCHED (tier-1-only region) - fixture: T17's repeated-demo multi-segment
set with fetching suppressed beyond segment N (T42's real fixtures upgrade
this later); check: drag/click behavioral tests; check: status bar reflects
selection/range/progress (Vitest on store bindings); check: new rows walked
by T12.

**Deps:** T07, T09, T17, T18, T19, T21.

---

## T36 - Track management (collapse, reorder)

**Goal:** What section O's foldable-panel mechanics become under the unified
column: per-track collapse and drag-reorder, persisted in uiPrefs.

**Context:** features/02 section O (fold mechanics, persisted state - the
BEHAVIOR contract: per-surface show/hide with persistence survives; the
one-line-fold PRESENTATION is retired by S1); genre convention (04 gap
table); interaction spec: the concept-1 mock's track column ("track order
drag-to-reorder; per-track collapse caret") - caret on the track label
collapses to label-only height; drag on the label reorders whole tracks,
drop = swap position, no animation required. Track PINNING is explicitly
out of scope (no section-O contract; optional future ledger addition).
uiPrefs schema additions (`trackOrder: string[]`, `collapsed:
Record<trackId, boolean>`) are declared in `types/state.d.ts` here (T07
owns the mechanism, not the schema).

**Owns:** features/02 O rows (amended: fold -> track collapse; ledger).

**DoD:** check: collapse/reorder/persist walked; check: uiPrefs survives
reload (Vitest + one browser check); check: O rows' amended semantics
documented in-diff.

**Deps:** T21, T07.

---

## T37 - UX polish sweep (remaining findings)

**Goal:** Close the 04 findings not owned by earlier tickets; verify none
regressed.

**Context:** `04-ux-findings.md` complete table (S1-S8, K1-K8, F1-F7) vs
the amendments landed across T15, T19-T36 (each ticket's Amendments line;
no consolidated mapping exists - building it IS this ticket's first work
item). K1/K2/K3 land via T20's mechanism + page wiring; S4 completes as the
union of T26+T27+T31; S3 as the union of T19+T34+T35. Expected genuine
remainder: F2 leftovers on non-toolbar surfaces, K8 affordances outside
lanes, empty-state copy, contrast stragglers (F6), and anything a prior
ticket descoped or ledger-deferred (e.g. K7 if T32 deferred it).

**Owns:** the findings ledger closure - every 04 finding ends OWNED+landed,
OWNED+deferred (reason in the ledger), or REJECTED (maintainer sign-off via
the ledger PR, per the shared ledger convention).

**DoD:** check: a closure table appended to `04-ux-findings.md` mapping
every finding ID to outcome + landing ticket(s); check: T12 axe/census
clean across all four migrated pages (browser, viewer, flamegraph,
tokio-stats).

**Deps:** T21-T36, T41.

---

## T38 - Dual-UI raw switch (all pages)

**Goal:** ADR section 8 stages 1-2 for EVERY migrating page: legacy and new
versions both servable, with an always-visible "Switch to new UI" / "Switch
to legacy UI" control on both. Raw switch - maintainer decision 2026-07-08:
NO view-state porting across the switch.

**Context:** ADR section 8 (three-stage plan + the raw-switch note); T02's
static-copy list serves legacy pages; telemetry-free (no usage tracking
exists or is added).

**Owns:** rollout mechanism (no feature rows). Consumed by T13, T14, T41,
and the viewer (T21).

**Work:** routing per the shared decision: `?ui=new` on the canonical URL
selects the new version (server-side or entry-script dispatch - implement
the simplest that works with the T02 static-copy serving). Switch control =
small persistent element (header/toolbar corner, visible without
scrolling, both UIs). Legacy injection mechanism, explicit: each legacy
HTML file gets ONE added line - a `<script src="ui-switch.js">` include
rendering the control; this is the only edit legacy pages ever receive
(their parity baseline is re-recorded once after it). Precedence:
explicit `?ui=` param > localStorage preference > default (legacy until the
flip). Switching preserves the page's QUERY STRING (trace source - else the
loaded trace is lost) but ports no view state: hash state is dropped going
to legacy; legacy -> new opens the default view. Default flip = one-line
change left unmade (maintainer decision). Note: ADR section 8's prose
stages are referred to here as stages 1-3 for shorthand; the ADR text is
authoritative.

**DoD:** every page with a migrated version shows the switch on BOTH
versions, always visible (T12 census asserts the control's presence);
round-trip switch keeps the same trace loaded on all pages; no view state
leaks across (test: zoom in new viewer, switch, legacy opens default view);
N10 deep links work in both.

**Deps:** T02. Blocks: T13, T14, T41 (they land WITH the switch, not
after).

---

## T39 - Legacy removal + final gate

**Goal:** ADR section 8 stage 3 + the migration's definition of done.

**Context:** ADR section 8 stage 3; the final gate = the T12 layer set run
in full (T12 defines the layers); 2.8 acceptance (the 100 MB cap lifted -
`MAX_OPEN_BYTES` guard replaced by the resident-window budget, features/01
H4 amended). PRE-DELETION step, explicit: record the legacy baseline
fixtures (census dumps + behavioral-differ readouts for every page, per
T12's formats) in `ui/parity/baselines/` BEFORE removing legacy pages -
after deletion the differ runs against these recordings. "Large trace" =
T42's >=100 MB multi-segment fixture set (T42 is a dependency).

**Owns:** the capstone verification; features/01 H4 amendment.

**Work:** record baselines; delete legacy pages + static-copy list +
`ui-switch.js` injection + inline-script remnants; lift the cap (browse
warning becomes "opens with windowed loading"); run the FULL T12 suite:
every inventory row across all four pages, census, behavioral differ vs
the recorded baselines, and the MEASURABLE NFR budgets - N1-N4, N6, N19 +
the ~10x heap budget - on demo + x8 stress + the T42 large set (the
process NFRs N7-N18 are review-verified against the checklist, not "run").

**DoD:** check: the ADR's definition of done - no inline script beyond
module tags, functional contract holds (full row-walk report committed to
`docs/ui-inventory/`), measurable NFR budgets hold with numbers recorded;
check: baselines committed before deletion (git history proves order);
check: every `docs/tickets/ledger.md` entry carries its sign-off PR per the
shared ledger convention; check: ADR status note updated.

**Deps:** T38, T42 + bake time (maintainer-declared).
