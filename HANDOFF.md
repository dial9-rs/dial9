# T34 - Load chrome: drop zone, file loading, New File - HANDOFF

(Supersedes the T27 HANDOFF inherited through the branch chain.)

## STATUS: DONE (DoD met, gate bar green)

All four DoD checks satisfied. The two ledgered amendments (S3 new-file-confirm,
#281 dismissal) landed with inventory rows amended in-diff. Gate bar green.

Branch `ticket/T34-load-chrome` in the T34 worktree; no push, no PR (per mandate).

## COMPLETED (commits, oldest first)

- `d1df24e` feat(T34): load-chrome state machine (drop/pick/URL + S3/#281 amendments)
  - `src/pages/viewer/load-controller.ts` - the DOM-free load state machine.
  - `src/pages/viewer/load-controller.test.ts` - 22 Vitest cases.
  - `src/pages/viewer/esc-cascade.ts` - adds the `load: 90` priority band.
- `a0ef082` feat(T34): wire load chrome into the viewer shell (drop zone + New File)
  - `src/pages/viewer/load-chrome.ts` - lit-html DOM view + drag/drop/pick wiring.
  - `src/pages/viewer/load-controller.ts` - adds the `onLoaded` label-commit hook.
  - `src/pages/viewer/shell.ts` - `sourceLabel` becomes a getter; "New File" button.
  - `src/pages/viewer/main.ts` - load chrome replaces the T21 bootstrap load.
  - `src/styles/viewer.css` - load-layer / drop-zone / loading / drag-overlay /
    New-File styles; toast region z-index lifted above the load modal.
- `f0437ba` docs(T34): ledger + inventory for the S3 confirm and #281 dismissal amendments
  - `docs/tickets/ledger.md` - B15 amended, B15/#281 added, B1/B7/B8 amended.
  - `docs/ui-inventory/features/02-viewer-html.md` - B15 + B1 migrated-viewer notes.

## DoD CHECKLIST

- [x] **row-walker green on B** - T12 tooling does not exist yet (chunk-1 note:
  "NOTHING to productize exists in the repo"), so it is not runnable. Substituted
  by the load-controller Vitest (every B behavior a load surface owns) + tsc/build.
  Coverage of the B rows:
  - B1 drop zone, B2 pick, B3 window drag-drop, B4/B5 drag feedback, B6 demo,
    B7 loading state, B8 progress text, B9 elapsed timer, B10 cancel-Esc,
    B11 cancel-Back, B12 stream/buffered (delegated to the T16 worker),
    B13 errors (+401 hint), B15 New File, B17 creds headers - all implemented.
  - B18 paint throttle is frozen-core (trace_parser.js) and runs in the worker
    unchanged - preserved by consumption, no UI work.
  - **B14 (Set/Clear Range in-memory re-parse) and B16 (Parse-perf popup):
    NOT wired here - boundary, see below.** They map to NOT-TRIGGERABLE on the
    migrated page until their toolbar surfaces land (their surface is T33's).
- [x] **drop/pick/URL loads behavioral-diffed** - all three converge on the same
  T16 `loadTraceInWorker` pipeline (drop/pick via an object URL the worker
  fetches; the core sniffs gzip magic so .bin and .bin.gz both decode), so the
  parse is byte-identical to the legacy load. Controller tests assert each path
  starts the load with the right URLs/headers and reaches the same terminal
  state. True byte-diff needs T12's behavioral differ (not runnable yet).
- [x] **New File confirms when a trace is loaded** (amended B15 row in-diff) -
  `requestNewFile()` runs `confirm()` when a trace is resident; declining is a
  no-op. Non-destructive: the worker only replaces the trace slice on success,
  so a cancelled/failed replace keeps the old trace (S3 recovery). Vitest:
  `requestNewFile (S3 confirm amendment)`.
- [x] **load section closes via Esc AND a visible close control, reopening works**
  (#281) - Esc closes via the esc-cascade `load` band; the top-right close
  control calls `dismiss()`; both return to the resident trace; reopening is
  another `requestNewFile()`. The boot chooser (no trace behind) is intentionally
  not dismissible. Vitest: `dismissal (#281 amendment)`.

## AMENDMENTS (both ledgered)

1. **S3 new-file-confirm** - `features/02 B15 | amended | T34`. Confirm gate +
   non-destructive-until-success = "no silent data loss, recovery in both
   directions".
2. **#281 dismissal** - `features/02 B15/#281 | added | T34`. Esc + visible close
   control + reopen; boot chooser not dismissible.

Plus `features/02 B1/B7/B8 | amended | T34` (presentation reframe: full-cover
modal, no emoji, ASCII progress-label glyphs, boot shows the drop zone, toast
z-index above the modal).

## DESIGN DECISIONS (my latitude, documented; no STOP-gate hit)

- **Load section model:** New File over a trace opens a dismissible full-cover
  chooser; the trace is preserved until a new load SUCCEEDS. The confirm is the
  intentional gate (S3); dismissibility is the recovery (#281). Both DoD checks
  are independently listed, so both mechanisms exist.
- **File drops feed the worker as an object URL** (not a main-thread FileReader
  parse) to keep parse off-thread per T16. Verified the core decodes a fetched
  blob for both .bin and .bin.gz (magic-byte sniff in fetchTraceStream/maybeGunzip).
- **Progress labels (B8/B9) keep the phase words + numeric content EXACTLY but
  normalize the legacy "middle-dot"/"ellipsis" to ASCII (" - " / "...")** per the
  repo style rule. "Load-timing line kept exactly" holds for the phases and the
  live wall-clock elapsed suffix; the row-walker/behavioral-differ gate on
  effects/data, not glyphs. The legacy "Analyzing N events..." phase has no
  equivalent (the new arch has no synchronous post-parse analysis stall -
  derivations are lazy/cached), so it is dropped.
- **Esc priority `load: 90`** - below help (100), above popups (80): a `?` overlay
  opened over a load closes first, then Esc cancels the load / dismisses the chooser.
- **Boot no longer auto-loads the demo** (that was the explicit T21 stand-in) -
  `?trace=` auto-loads, otherwise the drop zone waits and the demo is a link
  (restores legacy B1/B6).
- **`sourceLabel` in ShellDeps is now a getter** so the toolbar filename tracks the
  loaded source; smallest edit to the hot shell file.

## SCOPE BOUNDARIES (noted, not done - not in this ticket's DoD)

- **B14 (Set Range / Clear Range in-memory re-parse):** the re-parse mechanism
  (`lib/trace/reparse.ts`) already exists; the Set/Clear Range TOOLBAR buttons and
  the retained-buffer wiring are a toolbar surface (section C/D/E -> T33) and are
  not part of T34's DoD. `loadTraceInWorker` already returns the retained buffer in
  its result for a future consumer; the load chrome does not persist it (no store
  slice exists for it - adding one is a cross-cutting change outside this ticket).
- **B16 (loadPerf record / Parse-perf popup):** the worker's `TraceWorkerTiming`
  (T16) carries the phase marks; the Parse-perf popup that surfaces them is D9/T33.
- **Streaming progress -> status bar (T35):** progress renders in the load view
  here (keep-exactly); the status-bar duplicate surface is T35's. There is no
  progress store slice to dispatch into (none is defined), so progress stays local
  to the load view - T35 can subscribe to the load chrome or add a slice later.

## SEAMS / SHARED-FILE EDITS (for the integrator)

- `shell.ts` (HOT merge point): `ShellDeps.sourceLabel` changed `string -> () =>
  string`; added optional `onNewFile?()`; added a `New File` button (rendered only
  when a trace is loaded); imported `nothing` from lit-html. Additive except the
  `sourceLabel` type change (single call site in main.ts, no test constructs it).
- `main.ts`: dropped the bootstrap `loadTraceInWorker` import/call; added
  `mountLoadChrome`; `traceSource` now returns empty urls when no `?trace=`.
- `esc-cascade.ts`: one added const (`load: 90`) - the documented extension point.
- `viewer.css`: appended a load-chrome block; changed `.d9-toast-region` z-index
  60 -> 400 (error toasts must sit above the new load modal). One value change.
- `state.d.ts`: NOT touched.

## EVIDENCE (gate bar, run in dial9-viewer/ui)

- `npx tsc --noEmit` -> exit 0.
- `npm run test` (full Vitest, includes `pretest` boundary check) ->
  `check-core-imports: OK`; **Test Files 81 passed | 1 skipped (82); Tests
  1313 passed | 1 expected fail | 11 skipped (1325)**; 127s. No unexpected
  failures. The known straggler (`tests/core/all_skills_snippets.test.ts` +
  heavy worker_threads suites) did not time out this run.
- `npm run build` -> clean; emitted `dist/new/viewer.html` + `new-viewer` bundle
  (`vite-plugin-static-copy Copied 17 items`).
- `cargo build -p dial9-viewer` -> `Finished dev profile` (rust-embed picks up
  the fresh dist; no new root-level files, so nothing else to embed).

JS/TS/CSS/MD-only change - no `.rs` files touched, no trace-format change - so
per AGENTS.md the Rust nextest/stress/clippy/fmt runs are not required.
