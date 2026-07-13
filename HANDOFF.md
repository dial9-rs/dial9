# T36 - Track management (collapse, reorder) - HANDOFF

(Supersedes the T33 HANDOFF inherited through the branch chain.)

## STATUS: DONE (DoD met, all gates green)

Per-track collapse + drag-reorder under the unified track column, persisted to
`uiPrefs` and surviving reload. Section O's fold BEHAVIOR (per-surface show/hide
+ persistence) is preserved; the one-line-fold PRESENTATION stays retired by S1.
Pinning is OUT (as fenced). Per-track render delegation is UNCHANGED.

## COMPLETED (commits on `ticket/T36-track-mgmt`)

- `ae4f3f5` feat(viewer): declare uiPrefs.trackOrder + collapsed (T36 schema)
- `5ea10a0` feat(viewer): per-track collapse + drag-reorder, persisted (T36)
- `5efb230` test(viewer): track collapse/reorder/persist + reload round-trip
- `e8dfad5` docs(T36): amend section O for track collapse/reorder + ledger

## WHAT LANDED

- **Schema** (`src/types/state.d.ts`): `uiPrefs.trackOrder: readonly string[]` +
  `uiPrefs.collapsed: Readonly<Record<string, boolean>>`, additive (T25/T33/T26
  precedent). `panelCollapsed` left as a superseded no-op holder. Full-slice
  fixtures kept complete (`exhaustive.test.ts`, `store.test.ts`), plus the
  viewer store's resting default (`pages/viewer/store.ts`: empty order, nothing
  collapsed = the S1 "all surfaces visible" default).
- **Mechanism** (`src/pages/viewer/track-management.ts`, NEW - owned): the
  manageable set (cpu/queue/spans/events = section-O scope; timeline/lanes/
  task-detail pinned), order resolution robust to stale/partial orders,
  swap-reorder (`drop = swap position`), collapse predicate + `COLLAPSED_TRACK_H`
  (24, legacy O1), store actions (toggle/reorder with no-thrash guards), and
  localStorage persistence (`dial9.viewer.trackPrefs` JSON blob; hydrate on boot
  + persist on change; try/catch in-memory fallback mirroring legacy).
- **Column integration** (`tracks.ts`): ordered + collapse-aware visible list;
  `repeat` keyed by track id so a reordered canvas MOVES with its track (no
  cross-track pixel bleed); shell-owned manage wrapper (caret + grip in a
  reserved label-gutter strip, pointer-events:none except the two controls so
  the spans copy buttons stay live); `sizeTracks` skips a collapsed track and
  re-paints from CURRENT windowed state on re-expand (carried T17-audit 6-7).
- **Wiring** (`shell.ts`, `main.ts`): thread actions into the track column;
  hydrate before first paint + mount the persistence subscriber (dispose on
  unload). `viewer.css`: caret/grip strip + collapsed body-hide (legacy
  label-only). Per-track render delegation (the id-keyed branches) unchanged.
- **Docs**: `features/02` section O rewritten (O1-O7 amended inline, O8 added)
  with an AMENDED banner; ledger entries for O1/O2/O3, O4, O5/O6/O7, O8, and the
  schema additions.

## DoD

- check: collapse/reorder/persist walked -> Vitest
  `src/pages/viewer/track-management.test.ts` (23 cases): manageable set, order
  resolution (empty/permute/robust-to-stale), collapse predicate, swap-reorder,
  store actions, collapse height + re-expand-membership (T17 obligation).
- check: uiPrefs survives reload (Vitest + one browser check) -> Vitest
  "persistence: uiPrefs survives reload" (save -> fresh store -> hydrate restores
  order + collapse; + malformed-blob and storage-blocked fallback). BROWSER
  CHECK: manual (see REMAINING) - node has no DOM/localStorage; the persistence
  logic and store actions are fully unit-tested and the wiring is bundled
  (verified: `d9-track-manage`/`trackPrefs`/`d9-track-caret`/`d9-track-grip`
  present in `dist/assets/new-viewer-*.js` and `.css`).
- check: O rows' amended semantics documented in-diff -> `features/02` section O
  + `docs/tickets/ledger.md`.

## EVIDENCE (gate bar)

- `npx tsc --noEmit` -> exit 0.
- `npm run test` (full Vitest, single process) -> Test Files 87 passed | 1
  skipped; Tests 1392 passed | 1 expected-fail | 11 skipped. No unexpected
  failures. (The 1 expected-fail is the pre-existing `.fails` flamegraph-search
  checkpoint, unrelated. Known stragglers - `all_skills_snippets` + heavy
  worker_threads suites - passed in this run; re-run in isolation if they ever
  time out under full-parallel load.)
- `npm run build` -> clean (`built in ~0.5s`, 17 static items copied).
- `cargo build -p dial9-viewer` -> Finished (rust-embed picks up the rebuilt
  `dist/`).

## REMAINING (manual, not blocking)

- **One browser check** (DoD): in a real browser on `?ui=new`, collapse a track
  via the caret, drag-reorder two tracks via the grip, reload the page, and
  confirm the order + collapse state persist. The mechanism is fully unit-tested
  (round-trip) and bundled; this is the DOM-drag confirmation the node env can't
  perform.

## BLOCKERS / QUESTIONS

None. No STOP-gate hit.

## NOTES (scope-fence, note-only)

- `uiPrefs.panelCollapsed` (legacy `Record<FoldablePanelKind, boolean>`) is now a
  superseded no-op holder (no live surface reads it; the new `collapsed` field is
  the live track-collapse state). Left in place per the additive scope fence
  rather than removed; noted in the schema doc + ledger. A future cleanup could
  drop it once nothing depends on it.
