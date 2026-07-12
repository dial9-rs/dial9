# T20 HANDOFF - Unified keyboard model

(Replaces the T19 HANDOFF inherited through the branch chain; T19's
record lives in the tree at base 009582d.)

## STATUS

COMPLETE - no STOP-gate hit. All scope items landed, all gates green,
DoD evidence below. Base: 009582d (integration/chunk-1 tip). Branch:
`ticket/T20-unified-keyboard`. No push, no PRs.

Session note: this ticket ran across three agent sessions (one budget
kill, one watchdog stall). Commit b43aa08 is the salvage decision on the
first session's two draft files (both KEPT as-is: coherent, matched the
ticket design); edc4cbe is a mid-work wip checkpoint committed by the
supervising session (completed and verified by 52c5bd7).

## COMPLETED (commits)

| sha | what |
| --- | --- |
| b43aa08 | salvage: keyboard.ts router + zoom-history.ts drafts (KEPT as-is) |
| 7e30fba | mechanism: palette, help overlay, goto-time, announce, lib/interact barrel, interact.css |
| 85da324 | Vitest: router + zoom stack + palette policy + goto-time (40 cases) |
| e5bb83e | flamegraph wiring: fg-keys.ts (`?`/`f`/`z`), exact+api mode hooks |
| edc4cbe | wip checkpoint (supervisor salvage; superseded by 52c5bd7) |
| 52c5bd7 | browser wiring: page-keys.ts + heatmap-keys.ts + main.ts mounts |
| 68f9424 | parity tooling: axe-scan --press, axe-keyboard.mjs + harness, keyboard-checks.mjs |
| a4886b9 | axe fixes: overlay h2 heading order, harness landmark |
| cfafd6c | docs: inventory amendments in-diff + 10 ledger lines |

## WHAT SHIPPED

Mechanism (`ui/src/lib/interact/`): focus-tolerant key router (K5;
DOM-free core, Node-tested), bounded zoom-history stack (K4; store/page
state, deliberately NOT URL state per 05-url-view-state.md), search
palette component (K1; mock semantics: clamped arrows, wrapped
Enter-cycling, Shift+Enter prev), unified help overlay component (K3;
per-page content sections, dialog a11y, focus capture/restore),
goto-time grammar (mechanism only - T23 wires `g`), announcer
(aria-live region + 1.6s toast; viewer announce() + mock say()
precedent).

Flamegraph page (BOTH modes): `?` help (features/03 F94 table as the
content baseline + the page's live keys), `f` fit (zoom reset WITHOUT
clearing search - the F44 invariant the Escape cascade cannot honor),
`z` zoom-undo (bounded view history). `/` and Ctrl/Cmd+F remain the
frozen widget's own (composition via defaultPrevented; nothing
intercepted or re-mapped; F41 exactly as before). Zoom restore composes
released widget behaviors only: synthetic contextmenu pops (the public
right-click zoom-out-one-level, F94) drain the stacks, zoomToPath
rebuilds, a reentrancy guard keeps intermediate pops out of the
history; exact mode settles the URL sync once, api mode stays
URL-silent (F180). Frozen core untouched.

Browser page: `?` help, `/` focuses the active tab's search input,
Enter submits the browse search from any controls-bar field (K2),
heatmap keyboard window selection (K2 fix direction = the viewer's
documented Shift -> arrows -> Enter state machine; the plot is now
keyboard-focusable, fixing K2's axe-serious scroll region; keyboard
confirm lands on the SAME finalizeSelection as the mouse path and the
in-progress band renders through the same transient drag channel;
announced).

Viewer keys (n/p, g, WASD timeline): mechanism only, per the ticket
boundary - T23 wires the viewer surfaces in chunk 2.

## GATES (all green, run at cfafd6c)

- `npx tsc --noEmit`: clean.
- FULL `npm run test`: 57 files passed | 1 skipped; 962 tests passed,
  1 expected fail (suite-declared), 11 skipped - the pre-existing
  baseline plus the 4 new lib/interact suites (40 cases).
- `npm run build`: clean.
- boundary check (`npm run check:boundary`): OK - no core imports
  outside lib/trace + lib/canvas.
- `cargo build -p dial9-viewer`: clean (rust-embed embed check). JS/TS +
  docs only - per AGENTS.md no nextest/stress/clippy required.
- Dev-server (:3101) and browsers killed after evidence collection.

## DoD EVIDENCE

### check: `?` opens help on every migrated page (enumerated)

`node parity/keyboard-checks.mjs --base http://localhost:3101`
(dev-server over built dist):

```
PASS  browser page (new/index.html)  (? opened: true, Esc closed: true)
PASS  flamegraph exact mode (new/flamegraph.html?trace=/demo-trace.bin)  (? opened: true, Esc closed: true)
PASS  flamegraph api mode (new/flamegraph.html?api=1&bucket=demo-traces&prefix=traces)  (? opened: true, Esc closed: true)
```

### check: axe clean on palette/overlay

Component harness (`node parity/axe-keyboard.mjs --fail-on minor`;
palette + overlay OPEN, real src/ components served by a throwaway
programmatic Vite dev server - the palette has no landing-time page
mount in chunk 1, T23 wires it):

```
Summary: 0 violations, 0 nodes   (PASS at --fail-on minor)
```

Two moderates were found and fixed en route (a4886b9): overlay title
h3->h2 (heading-order skip after the pages' h1) and a missing harness
landmark.

Live pages, overlay open vs closed delta (`axe-scan.mjs` with/without
the new `--press "?"`):

```
browser page:     closed 4 violations/29 nodes -> open 3/28; nodes added by opening: NONE
flamegraph exact: closed 6/13 -> open 4/11;                  nodes added by opening: []
d9-component nodes in violations: NONE (only pre-existing #d9-ui-switch, T38, in both scans)
```

All remaining violations are pre-existing legacy-ported markup (label
#range-from, h1 contrast, region landmarks).

### check: row-walker on touched rows (dev-server :3101, built dist)

features/01 on new/index.html (`--rows D9,F12,F13,F14,F15,G2`):

```
D9 VERIFIED | F12 VERIFIED | F13 VERIFIED | F14 VERIFIED | F15 VERIFIED | G2 VERIFIED
Summary: 6 rows - 6 VERIFIED. GREEN: zero FAILED
```

features/03 on new/flamegraph.html
(`--rows F41,F92,F93,F94,F95,F96,F157,F171,F176,F177`):

```
F41, F92-F96, F157: NOT-TRIGGERABLE (no recorded verdict - listed, not
  gated, per the shared verdict mapping)
F171 VERIFIED | F176 VERIFIED | F177 VERIFIED   (api mode green with
  the new key wiring mounted)
Summary: 10 rows - 7 NOT-TRIGGERABLE, 3 VERIFIED. GREEN: zero FAILED
```

### check: Vitest for router + palette filtering + zoom stack

`ui/src/lib/interact/{keyboard,palette,zoom-history,goto-time}.test.ts`
- 40 cases, green inside the full run (85da324).

### Inventory amendments in-diff + ledger

- features/03: F157 amended; F186/F187/F188 added (section N).
- features/01: D9, F12 amended; A8, A9, F21 added.
- docs/tickets/ledger.md: 10 T20 lines appended.
- NOTE: markers are dated `[2026-07-12]` (the actual amendment date;
  the ticket draft anticipated `[2026-07-11]` - the sessions were
  killed across the date boundary).

## T15-OVERLAP: files touched under src/pages/browser/

For the integrator merging on top of T15 (sortable-table / unknown-key /
bucket-filter / axis changes - NOT in this base):

- `src/pages/browser/page-keys.ts` - NEW FILE (no conflict possible).
- `src/pages/browser/heatmap-keys.ts` - NEW FILE (no conflict possible).
- `src/pages/browser/main.ts` - EDITED, the ONLY pre-existing browser
  file touched: 2 import lines + 2 mount calls
  (`mountBrowserPageKeys(ctx); mountHeatmapKeys(ctx);`) inside boot(),
  between mountHeatmapInteraction and mountRawView. If T15 restructured
  main.ts, re-apply these four lines anywhere after `ctx` exists - the
  placement is NOT load-bearing (both modules only register listeners /
  set plot attributes; no store-subscriber ordering constraints).

Semantic overlap notes:
- page-keys.ts reads `els.searchBtn.disabled` for the Enter gate: if
  T15's bucket-filter work changed the Search readiness formula, the
  Enter path inherits it automatically (defers to rendered state).
- heatmap-keys.ts reads `els.heatmapCanvas.clientWidth` +
  `browse.rows`/ROW_H: if T15's axis change altered canvas sizing, the
  keyboard path scales with it (no duplicated constants).
- raw-view.ts (T15's sortable-table landing zone) untouched; G2's Enter
  handler is as T14 shipped it.

Also touched OUTSIDE the browser page (no T15 contact):
`src/pages/flamegraph/{exact-mode,api-mode}.ts` + new `fg-keys.ts`;
`src/lib/interact/*` (new + salvaged); `src/styles/interact.css` (new);
`parity/axe-scan.mjs` (additive --press flag) + new
`parity/{axe-keyboard.mjs,keyboard-checks.mjs,fixtures/keyboard-harness.html}`;
docs (both inventories, ledger).

## JUDGMENT CALLS (flagged; none met the STOP-gate bar)

1. "Enter-cycling" (palette): the mock's implementation closes on
   Enter, but its placeholder says "Enter jump - Shift+Enter prev" and
   the ticket names the canonical semantics "palette Enter-cycling".
   Implemented: Enter activates AND advances the selection (wrapping),
   Shift+Enter activates and steps back, palette stays open; Escape or
   click-activation closes. Trivial to flip if review prefers
   close-on-Enter.
2. Ticket says browser "`?` help (exists)" and K3 claims "? opens help
   on viewer/browser": no legacy browser help surface exists at HEAD
   (the page's only key handler is G2's Enter, index.html:1814).
   Implemented as a new binding - the work item is the same either way;
   recorded in the ledger's A8 line.
3. `z`/`f` over the frozen widget API: there is no public zoom-state
   setter (zoomToPath appends and no-ops on empty; the Escape-cascade
   reset clears search first, violating F44 for fit). Chosen: compose
   the widget's released behaviors (synthetic contextmenu = its public
   right-click zoom-out) + zoomToPath, reentrancy-guarded. The
   alternative (unfreezing flamegraph.js for a resetZoomOnly()) was not
   taken - scope fence.
4. Heatmap keyboard selection spans ALL host rows (row narrowing stays
   pointer-only) and has no keyboard zoom analog - K2's stated minimum
   plus the viewer-state-machine precedent; cheap to extend.
5. Shift-to-start inherits the viewer's known quirk: `?` (Shift+/)
   while the plot is focused starts a selection first (Escape/blur
   cancels) - identical to the legacy viewer's Shift behavior.
6. Marker date [2026-07-12] instead of the drafted [2026-07-11] (see
   DoD evidence note).

## OPEN QUESTIONS (non-blocking)

- Palette Enter semantics (judgment call 1) - flip on review?
- parity layer additions (--press flag + two scripts + harness) are
  T12-owned surface: fold into ui/README.md's parity section when
  convenient (README edit deliberately not made here - outside the
  ticket's doc scope of inventories + ledger).
- K3's row in 04-ux-findings.md still claims "? opens help on
  viewer/browser"; the browser half did not re-verify (see judgment
  call 2). Left un-edited: 04 is the audit RECORD, not a contract file;
  the correction is carried by the ledger + A8's row text.
