# T35 - Minimap + status bar - HANDOFF

(Supersedes the T34 HANDOFF inherited through the branch chain.)

## STATUS: DONE (DoD met, all gates green)

Branch `ticket/T35-minimap-status` (worktree), based on `integration/chunk-1`
@ `aeb724b` (T34 merged). Two new persistent viewer surfaces landed: the
overview minimap and the status bar. No blockers.

## COMPLETED (commits)

- `7aac7b1` feat(T35): minimap density/coverage/geometry MODEL (pure, tier-1
  sources) + tests.
- `054ebac` feat(T35): minimap canvas + status bar COMPONENTS, shell/main
  wiring, viewer.css, POI derivation, tests.
- `0efc267` docs(T35): inventory section X (additions) + ledger + a T18
  `coverageSignal` fallback test.

## WHAT LANDED (against the ticket)

Owns: NEW inventory rows (ledger "additions"). Amendments: S8, S3(surface),
F7(status), 2.8 progress feedback.

Minimap (`src/pages/viewer/minimap.ts`, `minimap-model.ts`, `minimap-poi.ts`):
- Fills the shell's `.d9-minimap` host with a canvas overview of the WHOLE
  trace range + a draggable/clickable viewport box (04 S8 position context).
- Density from TIER-1 sources only (architecture 2.8), precedence: aggregate
  density on coverage `full` -> listing-metadata (T17 segment extents + gzip
  sizes) -> whole-trace event histogram -> flat band. Falls back off a
  `partial`/`none` aggregate (never presents a partial density as complete).
- Per-bin coverage distinguishes FETCHED (`parsed`) from tier-1-only
  (`listed`/`fetching`/`evicted`/`oversized`) with the same residency mapping
  as `overlay/readout.coverageAt`; a "partial" badge surfaces tier-1-only
  regions so an unfetched tail is never rendered as empty/complete (T17-audit
  notes 6-7 - the headline correctness requirement).
- POI ticks from the frozen detectors (`filterPointsOfInterest` via
  `deriveMinimapPois`) - the SAME source as T33's rail, no code dependency.
- Drag scrubs / click jumps, dispatching store viewport updates (never a direct
  render, F2); ArrowLeft/Right keyboard pan on the focusable region.

Status bar (`src/pages/viewer/status-bar.ts`): selection line + explicit clear
affordance (04 F7), view range + duration, segment fetch/parse progress from
the segments slice (2.8 feedback hard edge; spinner while fetching), the T19
copy-link button (04 S3 surface clause), and persistent key hints.

Shell (`shell.ts`): the `.d9-minimap` + `.d9-status` slots became empty ARIA
hosts the two components fill imperatively (toast/legend technique - the
declarative shell re-render never orphans their children); exposed as
`minimapRegion` / `statusRegion`. `main.ts` mounts + disposes both. Edits kept
localized to the two slot regions + wiring (hot-merge-point discipline).

## DoD CHECKS

- check: minimap navigates a multi-segment trace whose tail is UNFETCHED
  (tier-1-only) -> `minimap-model.test.ts` ("multi-segment tail UNFETCHED
  (headline DoD)") + `minimap.test.ts` ("navigating into an UNFETCHED tail"):
  the range spans the full 10-segment listing, the tail bins are `truncated`
  (not empty/complete), and a click at the tail moves the viewport into a still
  `listed` (unfetched) segment. PASS.
- check: drag/click behavioral tests -> `minimap-model.test.ts` (pure
  click/drag/clamp math) + `minimap.test.ts` (store-dispatch behavioral:
  click-center, drag-scrub keeping the grabbed point under the pointer,
  edge-clamp). PASS.
- check: status bar reflects selection/range/progress (Vitest on store
  bindings) -> `status-bar.test.ts` (selectionState F7, segmentProgress 2.8,
  statusViewModel range+duration). PASS.
- check: new rows walked by T12 -> inventory section X (X1-X14) added with
  `file:line`/`data-*` anchors; T12 does not yet exist as a running tool
  (chunk-1), so the rows are recorded for its walk, not executed here.

## GATE EVIDENCE (worktree `dial9-viewer/ui`)

- `npx tsc --noEmit` -> exit 0 (TSC_OK).
- `npm run test` (full Vitest, includes `check:boundary` pretest = OK) ->
  `Test Files 87 passed | 1 skipped (88)`,
  `Tests 1377 passed | 1 expected fail | 11 skipped (1389)`. Zero unexpected
  failures; the known straggler did not time out this run. The 38 T35 tests
  are in minimap-model (23), minimap-poi (4), minimap (5), status-bar (6).
- `npm run build` -> clean (`built in ~0.4s`, static-copy 17 items; the only
  warnings are the pre-existing trace_parser.js `fs/os/child_process`
  externalization notes).
- `cargo build -p dial9-viewer` -> Finished (rust-embed picks up `ui/dist`).

## SCOPE BOUNDARIES / NOTES (not blockers)

1. AGGREGATE DENSITY (T18) is a SEAM, not a live fetch. The viewer has no
   populated aggregate source wired (the dev-server has no agg context per the
   shared-decisions aggregation note; the T17 segment window itself is also not
   yet wired into the live viewer - both are chunk-2 integrations, consumed as
   OUTPUTS here). `mountMinimap` accepts an optional `aggregate()` supplier
   (defaults to null); the density model implements + tests the documented T18
   `full` -> trust / `partial`|`none` -> fall-back-to-listing rule, exercised
   with the real `coverageSignal` classifier. A later scope-aware ticket feeds
   real aggregates through the seam without touching the component. This is the
   boundary the ticket itself draws ("consume T18 outputs"); not a gate.
2. S3 URL-STATE remains T19's. The copy-link button copies the live URL and
   exposes a `beforeCopyLink` flush seam; the viewer's view-state<->URL sync
   (`bindViewStateToUrl`) is T19's clause and is not wired here (the ticket
   assigns only the copy-link SURFACE to T35).
3. Minimap navigation dispatches viewport updates DIRECTLY and is not recorded
   in T23's private zoom-history stack (so `z` does not undo a minimap pan).
   The ticket does not require it; recording would need exposing T23's
   `ViewportActions`. Possible future enhancement.
4. Browser-driven halves (canvas pixel rendering, real DOM pointer wiring, axe)
   are deferred to T12 per the established chunk-2 pattern (e.g.
   spans-track.test.ts). Covered here at the pure-model + store-dispatch level.

## FILES

New: `src/pages/viewer/minimap.ts`, `minimap-model.ts`, `minimap-poi.ts`,
`status-bar.ts` (+ their `.test.ts`). Edited: `src/pages/viewer/shell.ts`,
`main.ts`, `src/styles/viewer.css`, `docs/ui-inventory/features/02-viewer-html.md`
(section X), `docs/tickets/ledger.md` (3 addition lines).
