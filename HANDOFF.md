# T23 - Pointer + keyboard interaction state machine - HANDOFF

(Supersedes the T24 HANDOFF inherited through the branch chain.)

## STATUS: COMPLETE (runnable gates green; T12-tooling gates implemented, not runnable in this env)

All lane-area input is now `lib/interact` state machines dispatching store
actions (architecture 2.5). Owned rows H1-H13 implemented, plus the K4
(zoom-undo on `z`) and K5 (WASD on the timeline) viewer integrations. Handlers
dispatch store actions ONLY - they never render (F2) and never scan
O(allSpans) (F6: click resolution reuses T22's `resolveLaneClick` / the
`lib/trace/query` helpers).

## COMPLETED (commits on `ticket/T23-lane-interaction`)

- `b84b871` feat: pure state machines (pointer, wheel, kb-selection) + viewport
  actions with the K4 zoom-history stack; adds the additive `curNs` field to
  `transient.DragState`.
- `842fa1c` feat: `lane-interaction.ts` DOM binding + `main.ts` wiring +
  selection-overlay/viewport-controls CSS.
- `6b141e8` test: 60 Vitest cases over the machines (every transition + cancel).
- `468198c` perf: plain-scroll wheels bail before any layout read (H5).

## FILES

New (T23-owned):
- `src/lib/interact/pointer.ts` - pan/region/zoom drag machine, 3px intent,
  drag-vs-click discrimination (+ `pointer.test.ts`).
- `src/lib/interact/wheel.ts` - Ctrl/Cmd = zoom-at-cursor, plain = scroll (H5)
  (+ `wheel.test.ts`).
- `src/lib/interact/kb-selection.ts` - H9 keyboard region/zoom selection machine
  (+ `kb-selection.test.ts`).
- `src/pages/viewer/viewport-actions.ts` - pure zoom/pan/fit/region math (legacy
  parity) + store-bound actions + K4 zoom history (+ `viewport-actions.test.ts`).
- `src/pages/viewer/selection-overlay.ts` - H10 region box render surface
  (+ `selection-overlay.test.ts`).
- `src/pages/viewer/lane-interaction.ts` - the DOM binding (listeners, viewport
  controls H1-H4, selection overlay, router key bindings).

Edited (small, additive - merge points):
- `src/types/state.d.ts` - `DragState.curNs` (drives the H10 box from the store);
  `src/types/exhaustive.test.ts` updated to the new shape.
- `src/pages/viewer/main.ts` - mounts lane interaction; folds its key bindings
  into the router BEFORE the generic `?`/Escape fallbacks.
- `src/styles/viewer.css` - `.d9-selection-overlay` (+ `.zoom`),
  `.d9-viewport-controls`.
- `src/lib/interact/index.ts` - barrel exports for the new machines.

## ROW COVERAGE (features/02 H)

- H1/H2/H3 zoom in/out/fit buttons - rendered as the floating `#btn-zoom-in`/
  `#btn-zoom-out`/`#btn-fit` controls, wired to `viewport.zoom(0.5)`/`zoom(2)`/
  `fit()`. Minor conscious deviation: legacy `zoom()` called `clearToasts()`;
  the new shell uses PERSISTENT hint chips (F5), not hint toasts, so there is
  nothing to clear - error toasts are deliberately left alone.
- H4 viewport controls panel - floating bottom-right, buttons `stopPropagation`
  so a click is never a lane click.
- H5 wheel - Ctrl/Cmd zoom-at-cursor (factor 1.3), plain wheel scrolls the lanes.
- H6 drag pan - 3px intent, duration-preserving edge-carry clamp.
- H7 Shift+drag region - GESTURE only: commits the range to
  `selection.sidebarRange`. WHAT it opens (flamegraph/blocking/heap, and the
  "no CPU samples" toast) is T32's `sidebarRange` consumer - the ticket's H7
  seam. Deliberately NOT gated on `cpuSamples` here.
- H8 Alt+drag zoom - zooms the viewport to the region.
- H9 keyboard Shift/Alt selection - full machine (start/extend 5%/confirm/cancel),
  seeded at mouse-in-view else view centre, blocked while `sidebarRange` is
  retained, announced via A16.
- H10 selection overlay - `selection-overlay.ts` renders the blue/teal box from
  `transient.drag` / `transient.keyboardSelection`, and PERSISTS a retained
  `selection.sidebarRange` box until the sidebar (T32) clears it.
- H11 arrow zoom / H12 arrow pan - ArrowUp/Down zoom, ArrowLeft/Right pan 20%
  (extend the kb cursor instead while a selection is active).
- H13 Set/Clear Range - delegates to E3/E4 (the toolbar Set Range/Clear Range
  buttons), which is T33's surface. T23 keeps the viewport slice those buttons
  read correct; no T23-rendered control. Row-walker will read H13
  NOT-TRIGGERABLE until T33 lands the toolbar.
- K4 (`z` zoom-undo) + K5 (WASD) integrated via the router bindings.

## DoD

- check: Vitest on the state machines (every transition incl. cancel paths) -
  PASS. 60 tests across pointer/wheel/kb-selection/viewport-actions/
  selection-overlay.
- check: wheel-zoom coalesced to <= 1 render/frame (03 F2) - PASS at unit level:
  `viewport-actions.test.ts` "6 wheel-style zooms flush a single subscriber
  notification". The T12 perf-probe (Playwright) is the integration form.
- check: row-walker green on H rows / behavioral differ vs legacy on J2/J3/J4 -
  REQUIRES T12 tooling (Playwright + dev-server + browser); NOT runnable in this
  environment. Access paths are implemented per ROW COVERAGE above; selection/
  zoom/pan semantics reuse the legacy math verbatim and T22's `resolveLaneClick`,
  so parity is structural. Run the row-walker + behavioral differ against the new
  viewer once T12's harness is available (the DoD's mechanical closure).

## GATE EVIDENCE

- `npx tsc --noEmit` -> exit 0 (clean).
- `npm run test` (full Vitest) -> 1230 passed, 1 expected-fail, 11 skipped, and
  ONE failure: `tests/core/all_skills_snippets.test.ts > ... Detect tight loops`
  timed out at 120000ms (the recipe itself runs ~200s). PRE-EXISTING and
  UNRELATED to T23 (a trace-analysis skills-snippet recipe; touches no
  interaction code). Reported, not fixed (scope rule). My five new suites: 60/60
  pass in 300ms.
- `npm run build` -> clean; viewer bundle `new/viewer.html` + `new-viewer-*.js`
  (82 kB) includes the interaction layer. ("externalized for browser" warnings
  are pre-existing from `trace_parser.js`.)
- `cargo build -p dial9-viewer` -> Finished (rust-embed picks up the built UI).

## SEAMS / DEFERRED (not T23's scope)

- H7 region opening + the no-CPU toast -> T32 (consumes `selection.sidebarRange`).
- Lane-click `openStackFor` -> Poll Detail is T31's surface; the SELECTION is
  dispatched now, the popup lands with T31 (noted in `lane-interaction.ts`).
- T17 windowing (carried obligation, audit notes 6/7): click/region resolution
  reads the resident `trace` slice (the viewer currently loads the whole trace).
  T23 never fabricates data for out-of-window regions - it dispatches ranges;
  coverage surfacing is T24's `coverageAt` readout + T32's partial-window badge.
  When viewer segment-windowing lands, the click resolver must respect the
  truncated/oversized covering segment (the T22 resolver's input data).

## MINOR NOTES FOR REVIEW

- `z` records history per discrete view change including each wheel notch, so a
  wheel-zoom storm is undone notch-by-notch. Defensible as "view history";
  coarser granularity is a T37 polish call if wanted.
- Announcement strings are ASCII (no em-dash / arrow glyphs) while preserving
  the legacy semantics (they are feedback-only; no logic depends on them).
