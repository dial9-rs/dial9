# T29 - Queue depth track (+ S6 queue-legibility amendment) - HANDOFF

(Supersedes the inherited T30 HANDOFF from the branch chain.)

## STATUS

DONE. All owned M rows landed; the S6 zero-baseline fix and F3(queue) legend
landed; M7 dispatch landed; M7-list / M8-links render halves are
deferred-until-T31 (T31 not yet on this branch). All four gate bars pass.

## COMPLETED (commits on `ticket/T29-queue-track`)

- `5d1025c` feat(viewer): queue depth track with S6 zero-baseline fix (section M)
- `98a6eb8` test(viewer): queue-model + queue-track Vitest (DoD)
- ledger entries (M1/M4, M5, M6, M7) appended in `docs/tickets/ledger.md`
  (committed with this HANDOFF).

### Files

New:
- `dial9-viewer/ui/src/pages/viewer/queue-model.ts` - pure, Node-testable:
  `computeQueueData` (series, cached per trace), `queueScaleY` (the S6 scale
  fn), `buildQueueRenderModel` (legacy bucketing verbatim), `computeSpawnedTasks`
  (M7 derivation T31 renders), `deriveQueueWindow` (T17), `QUEUE_LEGEND` (F3).
- `dial9-viewer/ui/src/pages/viewer/queue-track.ts` - the controller:
  `rowTemplate` (label + legend + canvas), `paint`, `drawQueueCanvas`, the M7
  drag-select handlers + `commitRange` dispatch, `spawnedTasks` accessor.
- `queue-model.test.ts` (14 tests), `queue-track.test.ts` (11 tests).

Edited (smallest additive edits to shared shell files):
- `src/types/state.d.ts` - added `selection.spawnedTasksRange: TimeRange | null`
  (the M7 dispatch contract T31 renders).
- `src/pages/viewer/store.ts` - initial `spawnedTasksRange: null`.
- `src/pages/viewer/tracks.ts` - delegate the "queue" row template + paint.
- `src/pages/viewer/shell.ts` - create/wire/dispose the queue controller.
- `src/styles/viewer.css` - `.d9-queue-*` legend strip + wrap.
- `src/types/exhaustive.test.ts`, `src/store/store.test.ts` - the two
  slice-shape anchor literals gain the new selection field.

## DoD

- check: **row-walker green on ALL M rows** - the T12 browser row-walker is not
  runnable from this worktree; per-row disposition (verify with T12):
  - M1 Global queue area - VERIFIED (filled step area; S6 baseline).
  - M2 Max-local queue line - VERIFIED (orange step line).
  - M3 Active-task line (right y-axis) - VERIFIED (green line + `tasks:N`).
  - M4 Y-axis labels - VERIFIED (maxQ top; `0` at the visible baseline).
  - M5 Legend - VERIFIED (in-track ribbon; F3 encodings match the draw).
  - M6 Hover info - VERIFIED via T24's at-cursor contract (column-wide
    mousemove populates `transient.atCursor`; no corner div).
  - M7 Drag-select - DISPATCH landed (`commitRange` -> `selection.spawnedTasksRange`);
    the sidebar LIST render is **deferred-until-T31**.
  - M8 Spawned-task link click - **deferred-until-T31** (T31's link handler sets
    `selectedTaskId`, which already exists; the links render in T31).
  - M9 Expanded-panel-click DEAD - moot in the unified column (no fold);
    DEAD-confirmed re-derives VERIFIED per the row-walker mapping.
- check: **J7 behavioral-diff, same numbers, presentation ledgered** -
  `buildQueueRenderModel` ports `renderQueueChart`'s bucketing verbatim (same
  per-bucket max global/local, same `maxQ`, same active-task
  `maxTasks`/`startCount`); `computeSpawnedTasks` ports `showSpawnedTasks`
  (taskFirstPoll spawn proxy, inclusive bounds, group-by-loc sorted by count
  desc). Only the y-mapping + labeling/legend changed - ledgered (M1/M4, M5, M6,
  M7). Asserted by `queue-model.test.ts`.
- check: **zero-global renders a visible baseline** - PASS. Vitest on the scale
  function (`queueScaleY(0,...)` is strictly above the axis and equals
  `chartTop+chartH-ZERO_BASELINE_PX`; monotone; clamped) plus the render-side
  complement (`drawQueueCanvas` plots an all-zero global on the baseline, no
  vertex on the axis bottom). The **one T12 visual check** is deferred to T12
  (browser visual harness).

## EVIDENCE (gate bar)

- `tsc --noEmit` -> exit 0.
- `vitest run` (full) -> **Test Files 75 passed | 1 skipped; Tests 1228 passed
  | 1 expected fail | 11 skipped**. New suites: 25 passed. No unexpected
  failures. (`node scripts/check-core-imports.mjs` - the `pretest` boundary
  gate - OK.)
- `vite build` -> clean (`built in 390ms`, 17 static items copied; the fs/os/
  child_process externalization warnings are pre-existing from
  `trace_parser.js`, unrelated).
- `cargo build -p dial9-viewer` -> Finished (rust-embed picks up `dist/`).

## SEAMS / notes for downstream

- **T31 (inspector)** renders the M7/M8 surface: read
  `selection.spawnedTasksRange`, call the queue controller's `spawnedTasks(range)`
  (or `computeSpawnedTasks(data, range)` directly) to get the groups; render 5
  task-id links per group + an "N more" tail + the range duration; a link click
  dispatches `store.update("selection", { selectedTaskId })` (M8). The at-cursor
  readout (M6) is already the `transient.atCursor` contract T31 renders.
- **T22 coordination (q:NN legend, F3)**: the in-lane `q:NN` entry already lives
  in T22's `LANES_LEGEND` ("local queue (q:NN)"); this track did NOT edit the
  lanes. The queue track's own legend covers its own encodings.
- **T24 at-cursor**: no code added here - T24's overlay already computes the
  Global Q / Local max / Active tasks readout for the ns under the cursor across
  the whole column (that IS the M6 reroute).

## BLOCKERS / QUESTIONS

None. One design decision taken within M-row ownership + the "smallest additive
edits to state.d.ts" allowance: M7's range dispatches through a NEW dedicated
`selection.spawnedTasksRange` field rather than reusing `sidebarRange` (which is
region-analysis/flamegraph state, T32) - the two are distinct sidebar surfaces,
so conflating them would collide with T32 and mis-block keyboard selection.
Documented in the field's doc comment and the M7 ledger line.
