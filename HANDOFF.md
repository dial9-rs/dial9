# T31 - Inspector sidebar (tabs: poll/event/related/stack) - HANDOFF

## STATUS: implemented, gate-bar green. Commit is BLOCKED in this environment
(`git commit` denied - see BLOCKERS). All work is in the working tree, ready for
the maintainer to commit.

## SCOPE DELIVERED

Persistent inspector `mountInspector(host, store, { esc })` replacing the shell
placeholder. Tabs: **Task / Poll / Event / Related / Stack**, driven by the
selection slice, re-scoping in the same action (04 S4).

- **P (sidebar mechanics):**
  - P1: persistent inspector (concept-1); selection re-scopes content, no
    open/close of the whole surface. Ledgered (features/02 P1 amended).
  - P2 + P8: drag-resize handle -> `uiPrefs.sidebarWidth` (live) + persisted to
    `localStorage["dial9.viewer.sidebarWidth"]`, re-read on mount. **P8 flips
    DEAD -> implemented** (ledgered). Clamp [200px, 92vw] as legacy.
  - P3: the F7 "what is selected" status line is the sidebar-title analogue.
  - P4: tab families - `tabAvailability(selection)` enables/disables tabs;
    `preferredTab(selection)` auto-activates on a fresh selection.
  - P5: auto-narrow to 350px on a fresh single-event pin (yields to a
    user/persisted width). P7 body scroll is CSS (`overflow-y:auto`). P6
    auto-widen-on-flamegraph is part of the T32 range seam.
- **Q (event / related detail):** Q1 event KV (single + cluster), Q2 copy
  (flash check), Q3 correlation button -> Related, Q4-Q8 Related sections (field
  correlation, enclosing spans, same-span/task/type), collapsible (Q5), windowed
  load-more (Q6), row navigation (Q7 - span focus+center / event pin+center),
  empty placeholders (Q8).
- **R (poll detail):** R1/R2 Poll Detail - deduplicated blocking-sched (red) +
  CPU-profile (orange) groups by leaf frame, count/percentage/bar, 3-frame head
  + expandable tail. **The behavioral-diff DoD gate**, ported from legacy
  `showStackPopup`. **R6 retired via ledger** (handler-less summary row, G20
  pattern). R3-R5/R7-R9 are the T32 seam (see SCOPE DECISION).
- **S4:** the at-cursor readout (`transient.atCursor`, T24's contract) renders
  in ONE place in the inspector, on the transient channel; **T24's inspector
  stub call is removed** from `components/overlay/index.ts`. Surfaces the T17
  coverage state (partial/oversized window). Ledgered (I6/S4).
- **M7/M8:** the Stack tab renders `computeSpawnedTasks` (T29's derivation) for
  `selection.spawnedTasksRange` - 5 hex task-id links per spawn-location group +
  a "N more" tail; a link selects the task (M8 -> selectedTaskId).
- **Task tab:** T30's `createTaskDetailDerivation` textual detail (spawn loc,
  polls, wakes, lifetime, completion, N3 uninstrumented badge, N4 idle-stack
  count). Consumed, not reimplemented.
- **F7:** "what is selected" line + explicit `✕ clear` button; Esc registered
  with the esc-cascade at the `sidebar` priority band (clears poll/event/range
  before the entry's task-clear fallback - the legacy D9 order).

### New store contract (additive; T29 `spawnedTasksRange` precedent)
`selection.pollDetail: PollSpan | null` (types/state.d.ts) - the clicked poll
for the Poll Detail tab. Wired in `lane-interaction.ts` onClick from
`resolveLaneClick(...).openStackFor` (the merged code already computed it and
noted "the popup lands when T31 wires it"). Store default added
(pages/viewer/store.ts). Three existing full-slice test fixtures updated with
`pollDetail: null`.

## SCOPE DECISION (flagged for maintainer): R3-R9 region blocking-calls -> T32

The ticket says "Owns P/Q/R rows", but the MERGED CODE
(`lane-interaction.ts:14-18`) states: a Shift-drag / keyboard region commit
writes `selection.sidebarRange` and "WHAT opens for that range (flamegraph /
blocking / heap) is T32's." Per hard rule 1 (merged code is authoritative on a
ticket-vs-code conflict), the region-triggered Blocking-Calls / scheduling panel
(R3, R4, R5, R7, R8, R9 - legacy `showSchedPanel`, viewer.html:6008-6203) is
T32's rendering (T32 deps on T31). T31 lands the Stack-tab CONTAINER + the
poll-click R1/R2 + the R6 retirement; the Stack tab shows a documented seam note
when `sidebarRange` is set. **Confirm this split** (T31 = inspector shell +
poll-detail + R6 disposition; T32 = region blocking-calls/flamegraph/heap into
the Stack tab). If R3-R9 must live in T31, they are an unported follow-on.

## FILES

New:
- `src/pages/viewer/inspector-model.ts` - pure derivations (Poll Detail, Event,
  Related, spawned-tasks, tab availability).
- `src/pages/viewer/inspector.ts` - the `mountInspector` component.
- `src/pages/viewer/inspector-model.test.ts` - 15 model cases (auto-discovered
  by Vitest; NOT a demo-trace suite, so no `e2e-trace-tests.sh` registration).

Edited:
- `src/types/state.d.ts` (+`selection.pollDetail`), `src/pages/viewer/store.ts`
  (+default), `src/pages/viewer/shell.ts` (empty inspector aside + expose
  `inspectorRegion`), `src/pages/viewer/main.ts` (mount + dispose),
  `src/pages/viewer/lane-interaction.ts` (dispatch pollDetail),
  `src/components/overlay/index.ts` (remove T24 readout-stub call),
  `src/styles/viewer.css` (inspector styles).
- `docs/tickets/ledger.md` (R6 retired; P8, I6/S4, P1 amended).
- Test fixtures: `store.test.ts`, `exhaustive.test.ts`,
  `selection-overlay.test.ts` (+`pollDetail: null`).

## ARCHITECTURE NOTES

- Render split (architecture 2.2/2.3): the FRAME (status/tabs/body, incl. the
  heavy Poll/Related derivations) re-renders only on trace/selection/uiPrefs;
  the READOUT re-renders on the high-frequency `transient` channel into its own
  binding-free host - a hover never re-runs buildPollDetail/buildRelated.
- The shell renders an EMPTY `<aside class="d9-inspector">`; the component owns
  its interior imperatively (the toast-region technique), so shell re-renders
  never clobber it. Shell edit is localized to the aside region + an
  `inspectorRegion` handle, mirroring toastRegion/trackColumn.
- Trace-invariant lookups cached in `store.derived(["trace"], ...)` (F5).
- T17 (audit notes 6+7): the readout surfaces `coverage` (partial/oversized)
  rather than presenting a truncated window as whole - the chunk-2 consumer the
  audit required.

## EVIDENCE (gate bar - all four green)

- `npx tsc --noEmit`: **exit 0**.
- `npm run test` (full Vitest, after the final refactor):
  **Test Files 83 passed | 1 skipped (84); Tests 1331 passed | 1 expected fail
  | 11 skipped (1343)**. 0 unexpected failures; no straggler timeout (ran full
  parallel in 120s). inspector-model + overlay/readout suites: 22/22.
- `npm run build` (vite): **clean** (only pre-existing frozen-core
  "externalized for browser" warnings).
- `cargo build -p dial9-viewer`: **exit 0** (rust-embed picks up the rebuilt
  dist).

## REMAINING / NOT UNIT-TESTED HERE (browser-driven; Vitest env is `node`)

- Row-walker (T12) not runnable in this worktree; P/Q/R rows implemented to pass
  it but not mechanically verified here.
- Store/DOM-boundary halves (live tab activation, resize round-trip, Esc
  registration, navigateRelated/selectTask dispatch) are not jsdom-tested (no
  jsdom installed; env is `node`) - the pure model suite covers the logic; the
  browser-driven verification is the row-walker's, consistent with T27/T30.
- Cosmetic simplifications vs legacy: Poll Detail per-frame escalating indent
  rendered as fixed 8/16px; Related span-row focus chain is the single span, not
  its ancestor chain. Content/counts/expand behavior is faithful.

## BLOCKERS

- **`git commit` is DENIED** in this environment (deny rule `Bash(git commit *)`).
  No step could be committed. Work is in the working tree (the first step's
  contract+ledger edits were `git add`-staged; later edits are unstaged). The
  maintainer must commit. Suggested split:
  1. `feat(viewer): add selection.pollDetail contract + T31 ledger entries`
  2. `feat(viewer): persistent inspector sidebar (P/Q/R + S4/F7)`
