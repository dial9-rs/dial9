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
