# UX findings: dial9-viewer (audit of the current UI)

Input to the UX-improvement phase. Audience weighting per maintainer: internal
expert users; keyboard ergonomics is a first-class goal ("easy and handy", make
expert lives easier). Pain lenses set by maintainer: correlating panels, trace
browsing, feature discoverability, locating the moment.

## Method

Evidence gathered five ways, then judged by three independent lenses (expert
efficiency / information architecture / feedback + discoverability), then every
load-bearing claim re-verified live before entering this catalog:

1. Journey walks against the live UI (dev-server :3001, demo trace, Playwright):
   8 journeys inferred from the tool's own codified diagnostic skills (J1 cold
   triage, J2 worst-poll hunt, J3 locate a known moment, J4 follow a task, J5
   flamegraph work, J6 S3 browse, J7 queue buildup, J8 share a view).
   Screenshot corpus + measured probes (key-effect map, tab order, axe scan,
   deep-link test) in the session scratchpad (`ux/*.png`, `ux_results.json`,
   `verify_results.json`).
2. Issue-tracker + git-history mining (real-user friction, fixed vs open).
3. Comparative conventions: Perfetto, Chrome DevTools Performance, Firefox
   Profiler, speedscope, Tracy - verified against their docs/source; ten
   de-facto genre standards extracted.
4. Three-lens judge panel over the same corpus.
5. Verification pass: judge claims re-tested live; corrections below.

**Corrections applied (judge claims that were driver artifacts):** clicking a
poll DOES open a rich task-detail panel (task id, spawn location, poll/wake
counts, lifetime) - but only when the click lands on a task-bearing poll (4 of
5 probed positions yielded nothing); the Queue Depth fold opens normally; POI
"Next" does visibly jump. Findings below use the corrected reality.

## Genre-standard gaps (measured)

| Convention (tools having it) | dial9 today |
|---|---|
| WASD timeline nav (4/5) | absent; only arrow keys |
| Search with Enter-cycling (4/5) | flamegraph only; viewer + browser: none |
| One-action fit / zoom-to-item (5/5) | mouse-only button; no key |
| Zoom history / breadcrumbs (4/5) | absent |
| Overview minimap w/ viewport marker (4/5) | absent |
| Selection re-scopes detail panels (5/5) | partial: task-detail fold only |
| Synced cross-view highlight (5/5) | partial: selected-task polls only |
| View-state permalink (2/2 web tools) | absent (URL = file only) |
| Annotations pinned to trace time (3/5) | absent |
| Track pin/hide/reorder (4/5) | absent |

## Findings

Severity: task-blocking > friction > polish. Class `structural` = data
organization / mechanism missing, not cosmetics. Evidence keys: E=expert judge,
IA=architecture judge, D=discoverability judge, probes=measured, iss=issue.

### Structural cluster (triggers the reorganization track)

| # | Finding | Sev | Evidence | Journeys |
|---|---|---|---|---|
| S1 | Default layout hides every analysis surface: 4 panels collapsed to one-line strips while ~70% of the viewport is empty on cold open | task-blocking | J1-1 screenshot; IA matrix | J1, J2, J7 |
| S2 | "Locate the moment" has no mechanism: no go-to-timestamp control, relative-only axis by default, TZ toggle hidden (`display:none`), keyboard travel keys dead | task-blocking | probes; iss #137 (longest-open thread); IA | J3 |
| S3 | View state lives only in memory: URL never changes (measured), refresh/share loses the analysis, "New File" discards without confirmation, no recovery in either direction | task-blocking | probes deepLink; D1/D2; iss #281 | J8, J3, all |
| S4 | Selection re-scopes almost nothing: task-detail fold updates (verified), but Spans/Events/CPU panels ignore the selection, the sidebar opens only for polls with samples, and at-moment stats scatter across three screen corners | friction | verify_results; IA3/IA7 | J2, J4 |
| S5 | Worst-poll triage is a blind linear stepper ("0/74" + Next): no ranked list, no POI markers on the timeline, no keyboard binding for the most-repeated action | friction | J1-1; E2; IA5; iss #450 history | J1, J2 |
| S6 | Queue data is split-brained: local queue = unlabeled in-lane sparkline + cryptic "q:NN"; global queue = collapsed fold, invisible at zero (confused a live Tokioconf audience) | friction | IA6; iss #282; J1-1 | J7, J1 |
| S7 | Flamegraph is severed from time: separate page, no frame->timeline link, and the sample counts contradict on-screen (viewer button "8993" vs page "147 samples", verified) | friction | verify_results; IA10; iss #571 | J5, J2 |
| S8 | No overview minimap: once zoomed there is no position context and no coarse jump target | friction | J3-1; convention 4/5 | J3, J1 |

### Keyboard ergonomics (maintainer's stated bar)

| # | Finding | Sev | Evidence | Journeys |
|---|---|---|---|---|
| K1 | No search anywhere in the viewer: a named task/span cannot be found except by eye | task-blocking | probes ("/" dead); convention 4/5 | J4, J2 |
| K2 | Browser page is keyboard-dead: heatmap window selection is mouse-only, no key submits search; lanes/heatmap scroll regions not keyboard-focusable (axe serious) | task-blocking | probes; axe | J6 (gates everything) |
| K3 | Three pages, three vocabularies: arrows work only in viewer; "/" only in flamegraph; "?" opens help on viewer/browser but nothing on flamegraph (verified: ?, h, F1 all dead) | friction | probes; verify_results | all |
| K4 | No fit-to-selection or zoom-history key: overshoot means starting over ("f", "0", "+/-", Home/End all dead; browser heatmap has double-click-reset, viewer does not) | friction | probes; convention 5/5 + 4/5 | J3, J1 |
| K5 | WASD absent, and the only nav keys (arrows) collide with focused form controls (POI select eats arrows once focused) | friction | probes; convention 4/5 | J1-J3 |
| K6 | Tab order inverted and discontinuous: actions before inputs, 7 consecutive stops inside one date field, timeline region last, stray body-focus gaps | friction | tabOrder dumps | J6, all |
| K7 | Flamegraph frames not keyboard-traversable (arrows dead; genre supports arrow walk + Enter zoom) | friction | probes | J5 |
| K8 | Click targeting for task selection is fragile: 4 of 5 probed lane positions yielded no task; no visual affordance marks where clicking works | friction | verify_results | J2, J4 |

### Feedback and discoverability

| # | Finding | Sev | Evidence | Journeys |
|---|---|---|---|---|
| F1 | Primary controls invisible to keyboard/AT users: unnamed selects (axe critical), icon buttons with empty accessible names, unlabeled inputs, duplicate IDs | task-blocking | axe; tabOrder | all |
| F2 | Toolbar jargon with no tooltips: "Uninstrumented (39)", "Parse perf", "Worst first", bare "0/74" | friction | J1-1; D8 | J1, onboarding |
| F3 | Legend does not cover what lanes render: no Global Q entry, "q:NN" unexplained, swatch encodings do not match in-lane rendering | friction | J1-1; iss #282 | J1, J7 |
| F4 | Empty states teach nothing: viewer void gives no next step; browser empty-state has no attached actions (demo/drop-file live in a low-contrast footer) | friction | J1-1, J6-1; axe contrast | J1, J6 |
| F5 | Shipped capabilities leave no surface trace: #55 still open though Alt+drag zoom exists; hint chips cover 2 of ~8 gestures; one chip disappears after first use | friction | iss #55; screenshots | all |
| F6 | Serious contrast violations on all three pages (15 nodes on browser page) | friction | axe | all |
| F7 | No persistent "what is selected" status or explicit clear/close affordance on selection surfaces | polish | D6 (narrowed by verification) | J2, J4 |

## Works well (keep through any redesign)

- Viewer help overlay: mouse + keyboard sections, documents a real keyboard
  region-selection state machine (Shift -> arrows -> Enter) - above genre norm.
- Hint chips placed in the gesture's target area; Esc consistently cancels.
- Load feedback line ("294,465 events - 2 workers - 4.15s - loaded in 0.7s").
- Collapsed folds carry live aggregates (info scent even when closed).
- Toolbar advertises data volume before commitment ("Flamegraph (8993)").
- Click tooltip groups the right cross-domain data for one instant - right
  grouping, wrong container (transient).
- Browser heatmap explains its encodings inline and its select-window ->
  open/profile pipeline matches the genre minimap convention.
- Cmd/Ctrl+scroll zoom-at-cursor; Alt+drag zoom; Shift+drag region analysis.
- POI worklist concept (worst-first + counter) is the correct triage primitive;
  it lacks list, markers, and keys, not the idea.

## Notes for the contract

- Inventory drift: HEAD has grown a `#health-btn "Tokio Stats"` button (browser
  page) and an `aggregation_enabled` config flag not present in
  `features/01..03`. Inventories need a refresh pass when the UX contract is
  amended.
- The task-vs-data-location matrix (IA judge) and the full judge reports are in
  the session scratchpad; this catalog is the deduplicated, verified summary.

## Next step (gate)

Maintainer prioritization over S1-S8 / K1-K8 / F1-F7, and a decision on the
structural cluster: S1-S8 together justify a reorganization concept pass
(2-3 alternative layouts for the viewer's information architecture, mocked and
compared) before the affected pages' migration slices are specced.

## Findings closure (T37)

This table closes every finding above against the amendments that landed
across T15 and T19-T41. It was built by grepping each finding id (S1..S8,
K1..K8, F1..F7) across `docs/tickets/ledger.md` and the chunk files
(`chunk-1-foundation.md`, `chunk-2-viewer.md`, `chunk-3-post.md`); the cited
ticket is the one whose ledger line claims the finding.

Outcome vocabulary (shared ledger convention):
- `LANDED` - fixed; the landing ticket(s) are cited and their ledger line
  carries the amendment.
- `DEFERRED` - owned but not landed; the reason and the follow-up ticket are
  named. A deferral is not a rejection.
- `REJECTED` - a maintainer decision not to fix; requires sign-off by
  approving this ledger PR. None this sweep (see "For maintainer" below).

Verification note: the live T12 axe/census across the four migrated pages
(browser, viewer, flamegraph, tokio-stats) needs the seeded DDB dev-server
and is a documented trailing item (see the T37 HANDOFF). Structural a11y
(roles / labels / cursor affordances present in the templates) was verified
by inspection and is cited per row.

| # | Outcome | Landing ticket(s) | Closure note |
|---|---|---|---|
| S1 | LANDED | T21, T36 | Collapsed folds die with the unified track column (T21); per-track collapse defaults to EXPANDED not all-collapsed (T36 O4, "the S1 amendment"). |
| S2 | LANDED | T33, T25 | Goto-time `g` input in the toolbar time cluster gives "locate the moment" a mechanism (T33 E3/E4, the `g` action from T20); time-axis date qualification (T25 F1, S2/#137 family). |
| S3 | LANDED | T19, T34, T35 | Union: zoom->URL debounced sync + versioned view-state hash + copy-link (T19); New File confirm + #281 dismissible load with recovery both ways (T34 B15); status-bar view-range + copy-link surface (T35 X8-X14). |
| S4 | LANDED | T24, T26, T27, T29, T31 | Selection re-scopes in one action: at-cursor readout contract (T24), spans/events tracks dim to selection (T26/T27), queue hover routes to the at-cursor readout (T29 M6), persistent inspector re-scopes its content and unifies the at-moment stats into one surface (T31 I6/S4). |
| S5 | LANDED | T33 | The blind POI stepper is replaced by the concept-2 issues rail: ranked, sortable, keyboard-navigable, with a red-flags summary chip (T33 C2-C8). |
| S6 | LANDED | T29 | Global/max-local/active-task series share one visible zero baseline; queue-track legend matches the drawn series (T29 M1/M4/M5). |
| S7 | LANDED | T32 | Count contradiction root-caused and reconciled on-screen (cpuCountLabel); "Show in timeline" links a CPU frame to its time extent (F20); flamegraph embedded in the inspector (T32 S7/F15-F16). |
| S8 | LANDED | T35 | Whole-trace overview minimap with a draggable/clickable viewport box (T35 X1-X7). |
| K1 | DEFERRED | T20 (mechanism); viewer wiring UNCLAIMED | The search palette component (`createSearchPalette` + `palette.test.ts`) landed in T20, but no chunk-2 ticket wired it into the viewer: `createSearchPalette` has zero page callers and the viewer key router binds only lane keys / `n`/`p` / `g` / `?` / Esc (no `/`). T20 planned the palette to "index whatever query exposes per page" as a chunk-2 activation that did not occur. Wiring it (index tasks/spans/POIs via `lib/trace/query` -> `palette.open` -> selection + viewport dispatch) is feature-sized, past T37's polish fence. The viewer `?` help advertises `/` (help.ts:23), currently a dead key, left as-is pending the wiring decision. FOLLOW-UP: a viewer-search wiring ticket (see "For maintainer"). Browser time-range search and flamegraph F38/F40 search are separate surfaces and present. |
| K2 | LANDED | T20 | Heatmap keyboard window-selection (F21), Enter submits the search (D9), `/` focuses the search input (A9). |
| K3 | LANDED | T20, T41, T21 | One `?` help vocabulary on all four migrated pages: flamegraph (T20 F186), browser (T20 A8), tokio-stats (T41), viewer (T21 help.ts); `/` unified where a search input exists (T20 A9). T37 also makes the viewer's advertised `f` live (K4), so `f`=fit reads the same on viewer and flamegraph. |
| K4 | LANDED | T20, T23, T37 | Flamegraph `f` fit + `z` zoom-undo (T20 F187/F188); viewer `z` zoom-undo (T23, lane-interaction); viewer `f` fit bound in T37 to the existing `viewport.fit()` (the H3/`f` action the "Fit all" mouse button already drove), closing the viewer fit-key sub-gap the `?` help advertised. |
| K5 | LANDED | T20, T23 | Focus-tolerant key router (T20); WASD (`w`/`a`/`s`/`d`) plus arrows on the timeline (T23, lane-interaction). |
| K6 | LANDED | T21 | Tab order follows the task flow (toolbar -> minimap -> tracks -> inspector); enforced by the T21 tab-order-dump DoD. |
| K7 | DEFERRED | T32 -> T47 | Keyboard flamegraph frame traversal needs a frozen-core absolute-zoom API (`setZoomPath` / `zoomToNode`) not in `flamegraph.js`'s public surface; T32 deferred it and filed the T47 core-reshape line item (chunk-3-post.md). Escape-to-reset already works via `handleEscape`. |
| K8 | LANDED | T22 | Lane click-target cursor/hover affordance (T22). Outside-lane clickable surfaces carry cursor affordances: minimap viewport box, issues-rail rows, inspector links/resizer, status-bar copy-link (viewer.css `cursor:pointer`); browser host-cards, heatmap (`crosshair`), sortable headers, back-link (browser.css). |
| F1 | LANDED | T21, T20 | ARIA landmarks + labeled controls built into the shell as the axe-critical fixes (T21, 04 F1), enforced by the T12 axe gate; palette/overlay roles (T20: `role=dialog`/`combobox`/`listbox`). Live axe = trailing item; structural roles/labels verified in the templates. |
| F2 | LANDED | T33 | Tooltips on every toolbar control; "Parse perf" demoted into the info menu (D6/D7); Clear Range conditional (T33 D1-D3/E3-E4). Non-toolbar surfaces (status-bar, minimap, issues-rail, inspector) carry `title`/`aria-label` (verified). |
| F3 | LANDED | T22, T29 | Merged lanes legend covers every in-lane mark incl `q:NN` (T22 G19); queue-track legend matches the drawn series (T29 M5). |
| F4 | LANDED (viewer) + PARTIAL (browser) | T21, T34, T37 | Viewer empty state teaches the next step (T21); load surfaces reframed (T34). Browser: T37 lifts the empty-state status + footer text to WCAG AA so the demo/drop next-step affordances are legible. The deeper IA change (moving demo/drop actions into the empty-state body rather than the footer) is a browser-page layout change, feature-sized, noted as an optional follow-up (not landed). |
| F5 | LANDED | T21 | Persistent hint chips covering the gesture set (T21). |
| F6 | LANDED | T21, T20, T37 | Viewer contrast fixes in styles (T21); interact.css muted foregrounds lifted to 4.5:1 (T20); browser page footer (#444, ~1.75:1) + header meta / empty-state status (#666, ~2.8:1) lifted to AA (T37). Trailing: live axe confirmation across the four pages; the browser page's `#888` labels on `#16213e` (~4.48:1, borderline) flagged for the live scan. |
| F7 | LANDED | T31, T35 | Persistent inspector selection status line + explicit clear affordance (T31 F7/P1); status-bar selection line + clear (T35 X8-X14). |

Summary: 23 findings - 21 LANDED, 2 DEFERRED (K1 viewer-search wiring, K7
flamegraph keyboard traversal), 0 REJECTED. F4 lands for the viewer and is
partially closed for the browser (contrast legibility landed; the empty-state
IA restructure is an optional follow-up).

Trailing / follow-up items (do not gate this closure):
- Live T12 axe/census across the four migrated pages (needs the seeded DDB
  dev-server) - confirm F1/F6 clean and re-check the browser `#888`-on-dark
  labels noted in F6.
- K7 keyboard flamegraph traversal -> T47 (already filed).
- K1 viewer task/span search wiring -> new ticket (mechanism ready in T20).
- F4 browser empty-state IA (attach demo/drop actions to the empty state
  body) -> optional follow-up.

For maintainer (ledger-PR sign-off):
- No findings are REJECTED this sweep, so no reject sign-off is required.
- Two DEFERRALS need a maintainer decision on their follow-up:
  - K1 (task-blocking): the viewer never wired the T20 search palette. Decide
    whether to file the wiring ticket now and whether to hide the currently
    dead `/` entry in the viewer `?` help until it lands.
  - F4 (browser): decide whether the empty-state IA restructure is wanted or
    the T37 legibility fix is sufficient.
