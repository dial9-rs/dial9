# T30 - Task detail track - HANDOFF

(Supersedes the inherited T24 HANDOFF from the branch chain.)

## STATUS: DoD met. Mechanical gates green (tsc / build / cargo build /
## vitest). Live-browser items (T12 row-walker / behavioral differ / visual)
## deferred per the T21/T22/T24/T25 precedent. Ready for review.

The task-detail timeline track (features/02 section N) landed as a store-wired
track mirroring the T26 spans track: a selection-keyed `store.derived` cache
(the 03 F5 fix - legacy `renderTaskDetail` re-collected + sorted every poll of
the task on EVERY frame), a per-frame render-model memo, faithful N6-N12 canvas
geometry, and interaction that dispatches `selection.hoveredWakerTaskId` (the
G8 waker highlight the lanes consume). The derivation is exposed for T31's
inspector Task tab (the hybrid split).

## COMPLETED (commits, on branch ticket/T30-task-detail)

- `fe25ecd` feat(viewer): task-detail derivation + render model (pure model)
- `3690d46` feat(viewer): task-detail track component + shell wiring
- `87bdf18` test(viewer): task-detail model + track Vitest

Files:
- NEW `dial9-viewer/ui/src/pages/viewer/task-detail-model.ts` - the pure
  SELECTION-keyed derivation (`computeTaskDetailData` - N2 numbers, N7 waker
  labels, N4/N11 dump refs), the per-frame render model
  (`buildTaskDetailRenderModel` - N6 delay bands, N9 lifespan, N10 poll bars /
  coverage, N11 idle gaps) with N5/N8 hit/waker regions, `formatTaskDetailSummary`
  (N2 plain text), and the T17 `TaskDetailWindow` descriptor.
- NEW `dial9-viewer/ui/src/pages/viewer/task-detail-track.ts` - the store-wired
  controller `createTaskDetailTrack`: derived cache (F5) + render-model memo,
  `paint`/`drawTaskDetailCanvas` (N6-N12 verbatim colours), waker hover/click
  dispatch + N5 status. Exposes `createTaskDetailDerivation(store)` +
  `deriveTaskDetailWindow` for T31.
- NEW `*-model.test.ts` (21 tests) + `*-track.test.ts` (11 tests).
- EDIT `tracks.ts` + `shell.ts` - smallest additive wiring: extended the
  spans-track delegate seam with a `taskDetailTrack` delegate (template + paint).
- EDIT `src/styles/viewer.css` - task-detail-only styles (status readout, id
  label, wrap positioning).

## BLOCKERS / QUESTIONS

None blocking. One resolved ticket-vs-code discrepancy, recorded here for the
reviewer:

### `hoveredWakerTaskId` lives in the `selection` slice, not `transient`

The ticket prose says T30 dispatches `transient.hoveredWakerTaskId` (T24's
slice). The MERGED CODE places the field in `selection` (`types/state.d.ts:154`),
and T22's lanes CONSUME `selection.hoveredWakerTaskId`
(`components/canvas/lanes/index.ts:127`) while subscribing to the `selection`
slice (`:137`). The task's binding is explicit: "the store field IS the
contract ... Read the merged code ... how G8 consumes hoveredWakerTaskId."
Dispatching to `transient` would be a no-op for the lanes (they never subscribe
to it), so the G8 highlight would silently break. T30 therefore dispatches
`selection.hoveredWakerTaskId` - the field the lanes actually read. Resolved by
the merged code being authoritative (per the task), NOT a stop-gate.

## SCOPE / SEAMS

### Hybrid split honored (T31 seam)

This ticket owns the TIMELINE track (N6-N12 visuals + N5/N8 interaction) + the
DERIVATION. The TEXTUAL detail is exposed for T31's inspector Task tab via
`createTaskDetailDerivation(store)` (the store-cached selector) +
`computeTaskDetailData` / `formatTaskDetailSummary`. The `TaskDetailData` shape
carries every N2/N3/N4 field (pollCount, wakeCount, spawnLocation, lifetimeNs,
hasTerminate, isInstrumented, taskDumps). The track's own label gutter shows
ONLY the track name + a compact "Task 0x.." identity; it does NOT build T31's
tab.

### Deferred-until-T31/T32 (the T27/T29 established pattern)

- N4 idle-flamegraph MODAL + N11 async-stack-on-idle-click MODAL open
  flamegraph / sidebar surfaces (T31 inspector / T32 in-viewer flamegraph). This
  track EXPOSES the data (`TaskDetailData.taskDumps`, per-gap `IdleBand.dumps`,
  the idle `HitRegion.dumps`) and renders the N11 cross-hatch + pointer cursor,
  but does NOT build the flamegraph modal - no store field for it exists (that
  schema is T31/T06's) and building it would be scope creep. The waker-select
  click (N8) IS wired (it reuses `selection.selectedTaskId`).

### T22 lanes seam (do-not-touch honored)

The waker highlight is a DISPATCH into `selection.hoveredWakerTaskId` only; the
lanes' G8 consumption is untouched. Worker-id derivation is computed locally
(`collectWorkerIds`, mirroring the lanes' `deriveWorkerIds` via `lib/trace`) so
the track does not import from `components/canvas/lanes`.

### Shell shared point

`tracks.ts` + `shell.ts` got additive edits only, extending the existing
spans-track delegate pattern (a `taskDetailTrack?` param + one delegation branch
each). The selectionOnly track already existed in `track-layout.ts` TRACKS
(owner "T30"); no change there.

### T17 carried obligation (audit notes 6+7)

`drawTaskDetailCanvas` consumes a `TaskDetailWindow` and surfaces a truncated
edge (hatch band) / oversized segment ("partial window" badge) rather than
presenting a clipped poll list as the task's whole history.
`deriveTaskDetailWindow` reads the `segments` slice; the whole-trace shell
(empty segments slice) resolves to "complete". Mirrors the CPU track's CpuWindow.
The `docs/tickets/reviews/T17-audit.md` file is absent from this worktree (same
as noted in the inherited T24 HANDOFF); the obligation is satisfied directionally
from the findings baked into the merged code + the CPU-track precedent.

## DoD

- check: row-walker green on N -> the N rows are implemented + observably
  reachable (track renders on task selection N1; N5 status; N6-N12 visuals; N8
  hover/click). The T12 Playwright row-walker is a browser tool not runnable
  here - DEFERRED, same disposition as every prior chunk-2 HANDOFF.
- check: task numbers exact vs legacy (behavioral differ, J4) -> asserted by
  Vitest `task-detail-model.test.ts` ("N2 numbers exact vs legacy" +
  "formatTaskDetailSummary") against the legacy formulas; the live T12
  behavioral differ over the running demo page is the browser follow-up.
- check: derived-cache Vitest (selection change invalidates, pan does not) ->
  DONE, `task-detail-track.test.ts` "task-detail derived cache (F5)": pan
  same-ref, task change new-ref, waker-hover same-ref (no re-collect).
- check: waker-hover dispatch Vitest -> DONE, `task-detail-track.test.ts`
  "waker-hover dispatch (N8/G8)": hover writes selection.hoveredWakerTaskId;
  off-label + clearHover reset it; click selects the waker task.

## GATE BAR (hard rule 3)

- `npx tsc --noEmit` -> exit 0.
- `npm run check:boundary` -> OK (no core imports outside lib/trace + lib/canvas).
- `npx vitest run` (the two T30 suites) -> 32 passed.
- `npm run build` -> clean, built in 7.29s; `new-viewer` bundle 81.29 kB
  (includes the task-detail track).
- `cargo build -p dial9-viewer` -> Finished (exit 0); rust-embed picks up dist.
  JS/TS/CSS-only change (no `.rs`, no trace-format change) so cargo
  nextest/clippy/fmt skipped per AGENTS.md.
- Full `npm run test` -> 1202 passed | 1 expected-fail | 11 skipped, PLUS one
  unrelated straggler failure (see below). My 32 new tests are in the pass count.

## EVIDENCE (commands)

- `cd dial9-viewer/ui && npx tsc --noEmit` -> exit 0
- `cd dial9-viewer/ui && npx vitest run src/pages/viewer/task-detail-*.test.ts`
  -> 2 files, 32 tests passed
- `cd dial9-viewer/ui && npm run build` -> built in 7.29s, clean
- `cargo build -p dial9-viewer` -> Finished
- Full `npm run test` -> Test Files 1 failed | 72 passed | 1 skipped (74);
  Tests 1 failed | 1202 passed | 1 expected-fail | 11 skipped (1215), 426s.

## UNRELATED PRE-EXISTING FLAKE (reported per AGENTS.md, not fixed)

The full-suite run had ONE failure: `tests/core/all_skills_snippets.test.ts >
dial9-trace-recipes: Detect tight loops (many spans per poll)` timed out at the
120s per-test ceiling. This is NOT my change:
- It is a frozen-core SKILL-RECIPE snippet over the demo trace; T30 touches no
  core/skills code (only the viewer task-detail track + tracks.ts/shell.ts/css).
- Run in ISOLATION it PASSES: `npx vitest run tests/core/all_skills_snippets.test.ts`
  -> 78 passed | 6 skipped, in 143s. The single heavy recipe only exceeds the
  120s per-test timeout when competing with 70+ other suites all parsing the
  3.4MB demo trace in parallel (the vite.config comment calls these parses
  "ceilings for stragglers, not expected durations").
Disposition: environmental parallel-load straggler, pre-existing, out of T30's
scope. Flagged for the reviewer; not fixed here (no change was requested and it
is unrelated).

## BROWSER-DRIVEN FOLLOW-UPS (need T12 tooling, not runnable here)

- Row-walker on section N against the running new viewer page.
- T12 behavioral differ (task-detail numbers) new-vs-legacy on J4.
- One visual check of the timeline bands vs the shared time axis (A13 alignment).
