# T33 - Toolbar, file info, and the issues rail - HANDOFF

(Supersedes the T27 HANDOFF inherited through the branch chain.)

## STATUS: DONE (DoD met, all gates green)

Branch `ticket/T33-toolbar-rail` off `integration/chunk-1` (base 6d022e0).
The concept-2 issues rail replaces the blind POI stepper; the toolbar file
info / analysis buttons / time controls land with F2 tooltips; the goto-time
`g` accelerator lands (S2). No push, no PR (per mandate).

## COMPLETED (commits)

- 3cd56ce  feat(viewer): POI slice + issues-rail data model (poi.ts)
- 7885846  test(viewer): POI model + rail-count parity (DoD)
- a3ed15a  feat(viewer): issues rail + toolbar components
- 9f6fa11  feat(viewer): wire toolbar + issues rail into the shell
- 5167242  refactor(viewer): drop shell file-info fields superseded by toolbar
- (ledger + this HANDOFF committed next)

### What landed

Owned rows features/02 C, D, E + amendments S5, F2, S2(partial):

- **Issues rail** (`src/pages/viewer/issues-rail.ts`, 04 S5): ranked,
  sortable (worker/kind/time/duration column headers), keyboard-navigable POI
  list replacing the "0/74 Next" stepper. Filter dropdown (C2), `n`/`p` step
  via the T20 router (C4/C5), row click centers the viewport + selects the
  task (C7 `jumpToPoi` math ported), "N/total" position (C6). Left column of
  the shell body.
- **POI model** (`src/pages/viewer/poi.ts`): the frozen `filterPointsOfInterest`
  detector set consumed via `lib/trace/analysis` (T09 seam) - NO new detector
  (scope fence). Memoized on trace identity (cpu.ts precedent). Jump semantics,
  n/p stepping, four-column sort, red-flag counts, per-row formatting.
- **Toolbar** (`src/pages/viewer/toolbar.ts`): file info (C1), red-flags
  summary chip (existing detectors' counts), analysis buttons (D1-D3) with
  tooltips + conditional visibility + count labels, info menu (info-icon
  `<details>`) hosting the DEMOTED Parse perf + uninstrumented details
  (D4/D6, F2), Time / TZ toggles driving `uiPrefs.timeMode/tz` (E1/E2),
  Set/Clear Range (E3/E4), and the goto-time input wired to `g` (S2).
- **Store**: new `poi` slice { filter, sortKey, sortDir, index } (state.d.ts,
  store.ts); `PoiSortKey` type. Additive; shell subscribes to it. Two full
  StoreState test fixtures (store.test.ts, exhaustive.test.ts) got the slice.
- Shell/main wiring: toolbar fills the Analysis/Time slots, rail in the body;
  `n`/`p`/`g` registered in the unified router; analysis/range surfaces route
  through injected seams (toasts) until T31/T32/T34 land.
- Styles: `src/styles/viewer.css` T33 section (toolbar controls, red-flags
  chip, info menu, rail table + severity dots, sr-only helper).

## DoD - all checks pass

- **check: rail count parity** - with the demo trace, default filter "sched",
  worst-first, the rail lists **exactly 74** (== the legacy "x/74"). Proven
  structurally in `poi.test.ts` against an independently re-derived legacy
  pipeline, for ALL five filters (sched=74, long-poll=2, cpu-sampled=3069,
  wake-delay=36496, uninstrumented=7529). Sort is count-independent (pinned).
- **check: every toolbar control has a tooltip (F2)** - every `<button>`,
  `<select>`, `<input>`, `<summary>` and clickable rail `<tr>` in toolbar.ts /
  issues-rail.ts carries a `title` + accessible name (source census; the T12
  axe/census browser tooling is the mechanical run, outside this gate bar).
- **check: row-walker green on C/D/E** - T12's row-walker is a browser dev
  tool (not in the gate bar). Owned-row behavior is covered by the Vitest
  units; the ledger amendments are in-diff (per-row disposition recorded).

## GATE BAR - EVIDENCE

- `npx tsc --noEmit` -> exit 0 (clean).
- `npm run test` -> 83 files passed | 1 skipped; **1322 passed | 1 expected
  fail | 11 skipped**; `check:boundary` OK (no core imports outside
  lib/trace + lib/canvas - confirms the T09 seam is respected). ~122s, single
  process (never two at once). No unexpected failures.
- `npm run build` -> clean (150 modules; new-viewer bundle 119 kB / 39 kB gz).
- `cargo build -p dial9-viewer` -> Finished (rust-embed picks up dist).
- T33 suites in isolation: poi.test (21) + toolbar.test (6) + issues-rail.test
  (4) = 31 passed.

## REMAINING / SEAMS (not T33 scope)

- Analysis buttons D1/D2/D3 open the inspector/flamegraph region analyses -
  those SURFACES are T31/T32. The buttons call an injected `onOpenAnalysis`
  seam (a toast now); they deliberately do NOT set `selection.sidebarRange`
  (that would soft-lock keyboard selection H9 with no sidebar to clear it).
  When T32 lands, swap the callback in `main.ts`.
- Set/Clear Range (E3/E4) reparse the retained buffer - buffer retention +
  URL start/end are the load chrome (T34) / URL codec (T19). Buttons call the
  `onSetRange`/`onClearRange` seams (toasts) now. Clear Range shows only when
  the resident trace is already range-filtered (trace.timeFiltered).
- POI tick marks on the minimap are T35's surface. T35 consumes the SAME
  detector output via `lib/trace/analysis` (poi.ts `poiSourceFor` / the frozen
  `filterPointsOfInterest`) - no T33<->T35 code dependency, as specified.
- The rail dispatches viewport updates directly (not through T23's
  ViewportActions instance), so a rail jump is NOT pushed onto the `z`
  zoom-history stack; because it does not record either, `z` returns to the
  last committed view (a reasonable undo of a jump). Sharing one
  ViewportActions across lane-interaction + rail was out of scope.

## T17 CARRIED OBLIGATION (audit notes 6+7)

The detectors run over the RESIDENT `trace` slice. The viewer shell loads
WHOLE traces today (segments slice empty), so the POI set / counts are
complete. When segment windowing feeds a partial trace (T34/T35 wiring), the
counts are over the resident window; consumers must not present them as
whole-trace truth. Documented in poi.ts header; no windowing UI built here
(downstream), matching the cpu.ts precedent.

## BLOCKERS / QUESTIONS

None. No STOP-gate hit. Two design calls made without a maintainer gate
(recorded in the ledger, reversible):
1. C3 "Worst first" checkbox is SUBSUMED by column-header sorting (the
   duration-desc default == worst-first; time-asc == chronological). The
   discrete checkbox is not reproduced - the ranked, directly-sortable list is
   the S5 amendment's intent.
2. Analysis/range buttons use injected seams (toasts) rather than dispatching
   store state, to avoid the H9 soft-lock. Reversible when T31/T32/T34 land.
