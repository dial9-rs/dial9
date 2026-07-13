# T32 - In-viewer flamegraph + region analyses - HANDOFF

## STATUS: DONE. DoD met; gate bar green. Committed on `ticket/T32-flamegraph-region`.

The Shift+drag region -> flamegraph / blocking-calls / heap panel is embedded in
T31's inspector Stack tab, driving the frozen `flamegraph.js` widget through its
existing public surface. The S7 amendment (count reconciliation + frame->timeline
link) and the T17 partial-window badge are landed. K7 keyboard traversal and
F17/F18 (idle / task-dump) are DEFERRED with ledger notes (see below).

## COMPLETED (commit shas on this branch)

- `88c1996` feat(viewer): region-analysis pure model (F15/F16/R3-R9 + S7)
- `46f7681` feat(viewer): embedded region-analysis panel + toolbar/inspector wiring
- (this commit) docs: ledger + F20 inventory row + T47 K7 line item + HANDOFF
  + region-analysis re-entrancy hardening

## SCOPE DELIVERED (features/02 section S + the R3-R9 seam from T31)

- **F15 CPU flamegraph (region/whole)** + **F16 heap flamegraph** render EMBEDDED
  in the inspector Stack tab, not a show-on-demand sidebar. The frozen
  `createFlamegraph`/`setData`/`getZoomPath` surface (via `lib/canvas`) is driven
  unchanged; the region-scoped sample building, counts, and Horvitz-Thompson heap
  estimates (Bytes/Count toggle, hook-frame stripping) are the pure
  `region-analysis-model.ts`, ported verbatim from `showFlamegraph` /
  `showHeapFlamegraph`.
- **R3-R9 blocking-calls panel** (deferred from T31 by the merged lane-interaction
  seam): group-by leaf/full (R4), 5-color summary bars (R5), expandable full-stack
  sub-groups (R7), 5 example-poll jump links centering the viewport (R8). Ports
  `collectSchedSamples` + `renderSchedPanel` verbatim.
- **H7 "what opens by data present"**: `defaultRegionMode` (sched-only -> blocking,
  heap-only -> heap, else CPU); the sub-tab bar shows only modes with data (P4).
- **Toolbar D1/D2/D3** now open the analyses for the whole trace via the
  `onOpenAnalysis` seam T33 injected (`openWholeTrace`).
- **F19 Pop Out**: opens `flamegraph.html` with trace URL(s)/start/end/zoom paths.
- **S7 count reconciliation** (below) + **S7 frame->timeline link** ("Show in
  timeline" navigates to the zoomed frame's `[min,max]` sample extent). New
  inventory row F20 + ledger.
- **T17 partial-window badge**: `regionCoverage` over the segments slice; the
  chunk-2 consumer of T17's coverage signal the audit (notes 6/7) required.

## S7 COUNT ROOT-CAUSE (DoD: documented in the PR + reconciliation landed)

**Conclusive (single reading; no STOP needed).** The two counts measure different
units on the same trace:

- Toolbar **"Flamegraph (8993)"** = `trace.cpuSamples.length` (toolbar.ts /
  legacy viewer.html:2375) - ALL CPU sample RECORDS: on-CPU + off-CPU/scheduling
  (`source === 1`) + stackless samples.
- Flamegraph **"147 samples"** = `filterCpuSamples(...).length` - only the
  FOLDABLE subset: `callchain.length > 0 && source !== 1` (frozen
  `flamegraph.js:129`; verified against `attachCpuSamples` which routes
  `source === 1` to `schedSamples`).

Not a bug - different units. The toolbar "advertises data volume before
commitment" (04-ux-findings "Works well"); the flamegraph counts what actually
folds. **Reconciliation landed**: the embedded CPU panel labels its count
`"N samples (on-CPU, with stacks) of M CPU records"` (`cpuCountLabel`) so the two
never read as a contradiction on one screen. The T33 toolbar button label is a
correct volume indicator and is left unchanged (T33-owned surface; changing it
would be scope creep with no correctness gain).

## DoD CHECKLIST

- check: row-walker green on section-S rows -> the features/02 row-walker
  registry does NOT exist yet (only `parity/walkers/features01.mjs` +
  `features03.mjs`; the viewer page is mid-migration). Satisfied per the
  established chunk-2 pattern (T31/T29/T30 HANDOFFs): the section-S behavior is
  behaviorally verified by `region-analysis-model.test.ts` (19 cases: F15 counts,
  F16 HT estimates + mode mapping, R3-R9 grouping, H7 default, S7 extent + count
  reconciliation, T17 coverage). F1-F14 are frozen `flamegraph.js` behaviors
  consumed unchanged (covered by the existing flamegraph_export / flamegraph page
  suites). When the features02 walker lands, register F15/F16/F19/F20 + R3-R9.
- check: region-select flows behavioral-diffed (J2/J5) -> the pure model ports
  the legacy sample-building / grouping / counts verbatim; the model tests are the
  field-level diff surface.
- check: count root-cause documented + reconciliation landed -> above + ledger +
  `cpuCountLabel`.
- check: frame click "show in timeline" -> `frameSampleTimeExtent` +
  `region-analysis.ts` "Show in timeline" button; ledger + new inventory row F20.
- check: partial-window badge when the region exceeds the resident window ->
  `regionCoverage` + `coverageBadge`; tested with a synthetic segments slice.

## DEFERRALS (ledgered)

- **K7 (keyboard frame traversal) -> T47.** The frozen `flamegraph.js` public
  surface has `getZoomPath` (read) + `zoomToPath` (append-from-EMPTY-stack, for
  URL restore) + `handleEscape` (cascade reset) but NO absolute zoom control
  (`setZoomPath` / `resetZoom` / `zoomToNode`), so keyboard traversal cannot drive
  the widget cleanly without editing the frozen core (forbidden). Deferred per the
  ticket with a T47 line item (chunk-3-post.md). Escape-to-reset-zoom already
  works via the widget's `handleEscape` in the inspector esc-cascade.
- **F17 (idle-time) / F18 (task-dump) flamegraphs -> follow-on.** Both are
  task-scoped (not range-scoped) analyses whose access paths are T30 (task-detail
  idle link N4) and T22 (lane idle-gap click N11) surfaces not wired at HEAD, and
  the selection-driven inspector does not model a task-scoped analysis surface
  without those dispatches. The region-panel rendering seam is ready to host them.
  The ticket GOAL (Shift+drag -> flamegraph/blocking/heap + S7) is complete.

## MERGED-CODE-VS-TICKET NOTES

- The R3-R9 blocking-calls panel is a **section R** row in the inventory but was
  DEFERRED to T32 by the merged `lane-interaction.ts:14-18` seam ("what opens for
  sidebarRange is T32's") and T31's HANDOFF. Picked up here (the region open is
  authoritatively T32's per the merged code, hard rule 1).
- **Whole-trace box:** the toolbar opens (`openWholeTrace`) reuse
  `selection.sidebarRange` = `[minTs,maxTs]` so the Stack tab activates (P4); the
  H10 selection overlay therefore boxes the full draw area (legacy toolbar opens
  drew no box). Intentional analyzed-scope indicator, ledgered. No new store field
  was needed (avoids state.d.ts / fixture churn).

## FILES

New:
- `src/pages/viewer/region-analysis-model.ts` - pure derivations.
- `src/pages/viewer/region-analysis.ts` - the `createRegionAnalysis` controller
  (widget lifecycle + sub-tab framework + imperative render into the host).
- `src/pages/viewer/region-analysis-model.test.ts` - 19 model cases (Vitest
  auto-discovered; not a demo-trace suite, no `e2e-trace-tests.sh` registration).

Edited (smallest additive):
- `src/pages/viewer/inspector.ts` - `regionPanel` added to `InspectorDeps`; Stack
  tab renders the binding-free `[data-region-host]`; `regionPanel.sync()` after
  each frame render.
- `src/pages/viewer/main.ts` - creates the panel, wires `onOpenAnalysis` ->
  `openWholeTrace`, disposes it.
- `new/viewer.html` - loads `/flamegraph.css` for the embedded widget chrome.
- `src/styles/viewer.css` - region-panel + blocking-calls styles.
- `docs/tickets/ledger.md` - F15/F16, R3-R9, S7, F20, F19, D1/D2/D3,
  partial-window, F17/F18 defer, K7 defer.
- `docs/ui-inventory/features/02-viewer-html.md` - new row F20 (frame->timeline).
- `docs/tickets/chunk-3-post.md` - T47 K7 core-API line item.

## EVIDENCE (gate bar - all four green)

- `npx tsc --noEmit`: **exit 0**.
- `npm run test` (full Vitest): **Test Files 88 passed | 1 skipped (89); Tests
  1403 passed | 1 expected fail | 11 skipped (1415)**. 0 unexpected failures; ran
  full-parallel in ~120s, no straggler timeout. region-analysis-model: 19/19.
- `npm run build` (vite): **clean** (only the pre-existing frozen-core
  "externalized for browser" warnings; new-viewer bundle grows with the embedded
  widget).
- `cargo build -p dial9-viewer`: **exit 0** (rust-embed picks up the rebuilt dist).

## NOT MECHANICALLY VERIFIED HERE (browser-driven; Vitest env is `node`)

- The live DOM flow (widget rendering into the inspector, sub-tab clicks, the
  show-in-timeline navigation, pop-out) is not jsdom-tested (no jsdom; env is
  `node`) - consistent with T27/T30/T31. The pure model covers the logic; the
  browser-driven check is the features02 row-walker's when it lands.
- S7 frame->time is CPU-mode only (heap would need alloc timestamps carried into
  the heap base samples - a small follow-on).

## BLOCKERS / QUESTIONS

None. K7 and F17/F18 are deferred with ledger + T47 notes (sanctioned outcomes),
not blockers.
