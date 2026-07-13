# HANDOFF - T25 Timeline header / time axis track

(Replaces the T21 HANDOFF inherited through the branch chain; T21's record
lives in the tree at base e0fa98e.)

## STATUS: DoD MET

The time-axis track (features/02 section F) is implemented, wired into the
T21 shell's "timeline" track slot, tested, and verified live in a browser.
All mechanical gates green; the date-qualification amendment landed with a
Vitest case, an in-diff features/02 F-row amendment, and a ledger row.

## What the axis renders

Fills `TRACKS[0]` ("timeline", T21 seam) with the F1 ruler:
- Nice-value tick interval auto-picked (`1e3..1e10` ns), targeting
  `max(4, floor(drawW/100))` ticks - legacy `renderTimeline` verbatim.
- `fmtTs`-parity labels: relative offsets (`+1.50s`) or absolute wall-clock
  (`HH:MM:SS` via nearest clock-sync anchor), falling back to relative when
  the trace has no anchor - legacy `fmtDuration`/`fmtWallClock` ported.
- Clock/format mode READ from `uiPrefs.timeMode` (E1) + `uiPrefs.tz` (E2);
  the axis never toggles - the buttons are T33's.
- Draw-area-relative x (F2 `nsToX`): the track canvas is `drawW`-wide and
  sits after the shared `LABEL_W` DOM gutter, so ticks align pixel-exact
  with the lanes/panels by construction (A13). NOT the legacy full-width
  canvas + inline `LABEL_W`.
- DATE-QUALIFICATION AMENDMENT: absolute-mode labels gain a `MM-DD ` prefix
  when the visible span crosses a calendar day in the active tz; same-day
  and relative mode stay time-only (T15 heatmap-axis rule, narrower prefix).

## COMPLETED (commits, off base e0fa98e)

- `0fd7983` feat(T25): axis render + clock modes + date qualification.
  New `src/pages/viewer/axis.ts`; `uiPrefs.timeMode`/`tz` added to
  `UiPrefsSlice` (T21 declared the seam, did not land it; defaulted to
  legacy rel/utc) + store default; `tracks.ts` dispatches the timeline
  track to `renderTimeAxis`; `shell.ts` feeds `deriveAxisInputs(state)`.
- `1fe56f8` test(T25): `src/pages/viewer/axis.test.ts`; in-diff features/02
  F1/F2 amendment; `ledger.md` features/02 F1 amended row.

Files: axis.ts (+354), axis.test.ts (+309), tracks.ts, shell.ts, store.ts,
types/state.d.ts (+timeMode/tz), store.test.ts + exhaustive.test.ts
(fixture fields), features/02-viewer-html.md, ledger.md.

## EVIDENCE (gates)

- `tsc --noEmit`: clean.
- `vitest run` (FULL): 64 files passed / 1 skipped; 1042 passed,
  1 expected-fail (pre-existing xfail), 11 skipped. 0 failed.
- axis.test.ts: alignment pixel-exact at THREE widths
  (pw 420 / 1024+sb15 / 2560) against lanes/cpu/queue/spans/events
  geometry; date-qualification (day-cross abs -> MM-DD, same-day/rel ->
  time only); fmtTs-parity labels; renderTimeAxis ruler.
- `npm run check:boundary`: OK (no core imports outside lib/*).
- `vite build`: success (only pre-existing os/child_process externalization
  warnings from the frozen trace_parser.js).
- `cargo build -p dial9-viewer` (CARGO_TARGET_DIR=.../target): success -
  rust-embed picked up the rebuilt dist.
- LIVE browser check (Playwright + vite preview on dist, ephemeral,
  not committed): timeline track drawW=980 == lanes drawW=980 (pixel-exact
  alignment in the real browser); canvas left edge at wrapLeft=100=LABEL_W;
  corner bg = (22,33,62) = #16213e (legacy F1 axis colour); 36 distinct
  sampled colours (ticks+labels rendered, not blank); ZERO console/page
  errors.

## REMAINING / notes

- Formal `walk-rows.mjs` features02 registry does NOT exist yet (T21 shipped
  only the ad-hoc `parity/verify-viewer-shell.mjs`; a viewer features02
  walker is T12/viewer-parity infra, out of T25 scope). F-row verification
  was satisfied via (a) the Vitest layout/format/render suite and (b) the
  live browser observation of the F1/F2 access paths above - the walker's
  function for F rows, not faked. If a features02 registry lands, F1/F2 slot
  straight into it.

## BLOCKERS: none

## Seam note for downstream

- `uiPrefs.timeMode`/`tz` now exist (rel/utc default). T33's E1/E2 toggle
  buttons drive them; T19's URL codec already carries `tm`/`tz` with the
  same "rel"/"abs"/"utc"/"local" vocabulary (reconcile at wiring time).
- T22 lanes MUST draw canvas-local x with the same `nsToX(ns, drawW)` form
  (= `nsToDrawX`) so they stay aligned with the axis; the alignment test
  locks this.
