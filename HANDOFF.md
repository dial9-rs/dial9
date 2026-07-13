# T24 - Crosshair overlay, hover tooltip, info readout - HANDOFF

(Supersedes the T22 HANDOFF inherited through the branch chain.)

## STATUS: DoD met (mechanical gates green; live-browser items deferred per the
T21/T22/T25 precedent). Ready for review.

The transient channel (features/02 I1-I6 + V) landed on the store's `transient`
RAF channel with the 03 F3 read-after-write anti-pattern eliminated. The
`transient.atCursor` readout store contract is defined and populated; a minimal
inspector stub renders it until T31 lands.

## COMPLETED (commits, on branch ticket/T24-crosshair-tooltip)

- `79a02c6` feat(T24): transient.atCursor readout contract in state types
  - `types/state.d.ts`: new `AtCursorReadout` interface (the store contract T31
    renders) + `atCursor` field on `TransientSlice`. Carries the T17
    windowed-data `coverage` signal.
  - `pages/viewer/store.ts`: init `transient.atCursor: null`.
  - `store/store.test.ts`, `types/exhaustive.test.ts`: extended the T06/T07
    StoreState literals with the additive field (mechanical, like T25's
    timeMode/tz).
- `d457bee` feat(T24): crosshair overlay + hover tooltip + at-cursor readout
  - `components/overlay/crosshair.ts`: pure `drawCrosshair` (I2 mouse / I3
    keyboard cursor / I4 custom-event guide / I5 pinned marker) + `crosshairX`
    using the SHARED `time.nsToPanelX` (A13/N4 invariant). Overlay canvas
    resizes only on geometry change (lib/canvas/dpr).
  - `components/overlay/tooltip.ts`: `placeTooltip` PURE over CACHED dims (the
    F3 fix), ResizeObserver-refreshed; `laneTooltipModel` reproduces the G16
    content from T22's `assembleLaneHover` output (consumed, not
    reimplemented); lit-html render (auto-escapes, #587).
  - `components/overlay/readout.ts`: `computeAtCursorReadout` (I6 parity) +
    `coverageAt` (T17 residency mapping) + `renderAtCursorStub` (inspector).
  - `components/overlay/data.ts`: once-per-trace hover-input derivation (global
    queue + active-task series LaneData omits), cached via store.derived.
  - `components/overlay/index.ts`: hover controller - mousemove reads geometry
    once (clean, pre-write) and dispatches transient; drawFrame (sole
    subscriber on hover frames) reads-before-writes -> zero forced layout.
  - `pages/viewer/main.ts`: mounts the overlay after shell+lanes.
  - `styles/viewer.css`: overlay/tooltip/readout styles + `.d9-track-column`
    positioning context.
- `0fe9320` test(T24): overlay vitest (crosshair/tooltip/readout).

## DoD

- check: row-walker green on I/V rows -> DEFERRED (T12 Playwright row-walker;
  live-browser, same disposition as T21/T22/T25 HANDOFFs). Behavioral fidelity
  is a faithful port of the legacy `renderCrosshair` / lane-hover handler /
  `updateInfoPanel`, locked by the `laneTooltipModel` content Vitest.
- check: T12 perf probe - hover storm ZERO forced-layout from the tooltip path
  -> DESIGN GUARANTEE + Vitest proxy DONE; full Playwright probe DEFERRED.
  Guarantee: `placeTooltip` is pure over cached dims (no element measure, ever);
  dims refresh via ResizeObserver (no forced sync layout); on a hover-only frame
  the shell does NOT run (never subscribes to `transient`), so `drawFrame` is
  the sole subscriber and its geometry reads precede all writes. `placeTooltip`
  purity asserted in tooltip.test.ts.
- check: crosshair alignment vs lanes (Vitest on shared layout math) -> DONE
  (crosshair.test.ts: overlay x == every track's absolute-in-column x at three
  widths). The "+ one T12 visual check" is the deferred live item.
- check: at-cursor store contract populated during hover -> DONE
  (readout.test.ts: computeAtCursorReadout + the transient.atCursor contract
  delivered to a subscriber on the coalesced tick; the stub renders it).

## GATE BAR (hard rule 3) - all green

- `npx tsc --noEmit` -> exit 0.
- `npm run test` (full Vitest) -> Test Files 68 passed | 1 skipped; Tests 1080
  passed | 1 expected-fail | 11 skipped. Zero unexpected failures. Boundary
  check (`check:boundary`) OK - no core imports outside lib/trace + lib/canvas.
- `npm run build` -> clean (129 modules; new-viewer bundle 48.30 kB).
- `cargo build -p dial9-viewer` -> Finished (rust-embed picks up the new
  dist/). JS/TS/CSS-only change: no `.rs` touched, no trace format change, so
  cargo nextest/clippy/fmt skipped per AGENTS.md.

## SCOPE NOTES / SEAMS

- T22 seam: consumed `assembleLaneHover` / `LaneHoverData` unchanged; T24 only
  assembles its INPUT (via `deriveOverlayData`, reusing frozen-core builders)
  and renders the tooltip. Lane hover LOGIC not reimplemented. `deriveWorkerIds`
  imported from lanes/data.ts. (One extra one-time `buildWorkerSpans` pass per
  trace load - overlay + lanes each derive; not a per-frame cost.)
- T23 seam (pointer machine, NOT landed): T24 owns the HOVER path only
  (`transient.mouseNs` + tooltip + readout). It adds a mousemove/mouseleave
  listener that dispatches transient; drag/zoom and the keyboard-selection
  cursor are T23's (`transient.drag` / `transient.keyboardSelection`, which the
  crosshair already reads). While a drag is active the controller suppresses the
  tooltip + mouse crosshair (legacy V3), so the two won't fight when T23 lands.
- T31 seam: `transient.atCursor` is the store contract T31's inspector renders;
  `renderAtCursorStub` is the placeholder that proves the wiring until then.
  It appends into `.d9-inspector-body` (static-content container -> survives the
  shell's declarative re-renders, the legend/toast technique).
- Shell shared point: `main.ts` got a 3-line additive mount + dispose; the
  overlay canvas is an imperatively-appended child of `.d9-track-column`
  (re-ensured each frame). No edit to `tracks.ts`/`track-renderers.ts`.

## BLOCKERS / QUESTIONS

None blocking. One observation:

- The binding-file `docs/tickets/reviews/T17-audit.md` does NOT exist in this
  worktree (the whole `docs/tickets/reviews/` dir is absent). The T17-audit
  FINDINGS are baked into the merged code (`types/state.d.ts` cites "T17-audit
  finding 2", `types/trace.d.ts` cites finding 1 + "notes 6-7"). I satisfied the
  carried obligation from those: `AtCursorReadout.coverage` + the tooltip
  "Window:" warning row surface a truncated/oversized window instead of
  presenting it as whole, populated from the `segments` slice residency
  (`coverageAt`). On the current whole-trace (non-segmented) load path the map
  is empty, so coverage resolves to "complete"; when a future ticket wires
  segment windowing into the viewer, the covering segment's state drives it. Not
  a stop-gate (the obligation is directional and satisfied); flagged for the
  reviewer's awareness.

## EVIDENCE (commands)

- `cd dial9-viewer/ui && npx tsc --noEmit` -> exit 0
- `cd dial9-viewer/ui && npx vitest run src/components/overlay/` -> 3 files, 29
  tests passed
- `cd dial9-viewer/ui && npm run test` -> 1080 passed, 0 unexpected failures
- `cd dial9-viewer/ui && npm run build` -> built in ~0.4s, clean
- `cargo build -p dial9-viewer` -> Finished
