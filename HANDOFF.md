# HANDOFF - T12 (Parity gate tooling)

## STATUS

DONE. All five verification layers (a)-(e) plus journeys J1-J8 landed and
verified against the live legacy pages; every locally-checkable DoD item ran
green. No open questions, no blockers.

## COMPLETED (commit shas, in order)

- `3542bdd` layer (a) row-walker verdict gate (committed by the previous
  session; re-verified this session, see EVIDENCE)
- `c091fb8` journeys J1-J8 + readout schema fixture + capture/diff lib +
  run-journey smoke runner (salvaged from the dead session's uncommitted
  debris after verifying all 8 journeys pass)
- `56b05ad` layer (b) affordance census dump + differ
- `5eb3580` layer (c) behavioral differ
- `843eefd` layer (d) axe + contrast scan
- `99c0f8e` layer (e) perf probe + pluggable render source (+ two
  step-executor fixes the storms surfaced, see below)
- (this commit) ui/README.md "Parity gate tooling" invocation docs + this
  HANDOFF

Branch: `ticket/T12-parity-tooling`, on top of merge `474d17c` (code chain +
docs branch). NOT pushed, no PR (per dispatch rules).

## WHAT EXISTS NOW (`dial9-viewer/ui/parity/`, dev-only, never in dist)

- `walk-rows.mjs` + `walkers/features01.mjs` - layer (a); registry covers
  features/01 (42 gated rows). Other inventories need their own walker
  registries when their page tickets arrive (T13: features/03; chunk 2:
  features/02).
- `journeys.mjs` - J1-J8 from 04-ux-findings' Method as declarative step
  lists; `lib/steps.mjs` is the step vocabulary/executor;
  `run-journey.mjs` smoke-runs them.
- `census.mjs` + `lib/census.mjs` - layer (b); dump (--url) or diff
  (--a/--b, exit 0 only on ZERO diff).
- `behavior-diff.mjs` - layer (c); same journey on two URLs, field-exact
  readout diff per checkpoint. Readout schema fixture:
  `fixtures/readout-schema.mjs` (owned by the parity tool).
- `axe-scan.mjs` - layer (d); violation list in 04-ux-findings' evidence
  shape + contrast summary; `--fail-on <impact>` gate mode.
- `perf-probe.mjs` + `lib/render-sources.mjs` - layer (e); journey ->
  interaction storm; records frames, long tasks, total/forced layouts
  (trace-based, DevTools forced-reflow signal), render invocations per
  frame via pluggable source. Only source today: documented `stub`
  (always unavailable). CHUNK 2 MUST wire a `store` source backed by the
  new store scheduler's devRenderAssertStats() dev hook (the hook is NOT
  in this tree).
- Shared plumbing: `lib/browser.mjs` (fixed 1440x900 viewport; browser-page
  clock pinned to dev seed date 2026-04-09T21:00Z), `lib/actions.mjs`,
  `lib/inventory.mjs` (verdict mapping), `lib/cli.mjs`, `lib/report.mjs`.
- Invocations documented in `dial9-viewer/ui/README.md` ("Parity gate
  tooling" section) - the DoD's documentation item.

Fixes landed in `99c0f8e` (found by the perf storms, affect shared libs):

- `lib/actions.mjs` waitBrowserBootstrap: a browse search WITH results hides
  `#browse-status` but leaves "Searching..." as its text (index.html:1239),
  so hidden now counts as settled. Only re-entrant loaded-waits ever hit
  this; row-walker re-ran green after the change.
- `lib/steps.mjs`: box() uses .first() (flamegraph page has a hidden second
  .fg-canvas); hoverSweep takes optional fractional-y and clamps the sweep
  line into the viewport (flamegraph canvas is ~3000px tall).

## EVIDENCE

All against dev-server :3021 (`npm run build` first; server:
`CARGO_TARGET_DIR=<repo>/target PORT=3021 cargo run -p dial9-viewer --bin
dev-server --features dev-server`; readiness gate: /api/config JSON).

DoD "row-walker on features/01 against the legacy page yields zero FAILED,
recorded-VERIFIED and DEAD-CONFIRMED re-derive VERIFIED":

- `node parity/walk-rows.mjs --inventory ../../docs/ui-inventory/features/01-index-html.md --url http://localhost:3021/index.html`
- Result: 75 rows - 42 VERIFIED, 33 NOT-TRIGGERABLE, 0 FAILED; exit 0
  ("GREEN: zero FAILED"). Re-run AFTER the actions.mjs fix: identical.
- Recorded-verdict -> tool-verdict distribution (from the run's JSON):
  - 41 x recorded VERIFIED -> VERIFIED
  - 1 x recorded DEAD-CONFIRMED (refresh) -> VERIFIED  (G8 dead sort)
  - 7 x VERIFIED-API (refresh) -> NOT-TRIGGERABLE (listed, not gated)
  - 10 x NOT-TRIGGERABLE (base+refresh) -> NOT-TRIGGERABLE
  - 5 x NOT-TESTED, 4 x PARTIAL, 4 x CODE-READ (refresh), 2 x CODE-ONLY,
    1 x NOT-OBSERVED -> NOT-TRIGGERABLE
  - FAILED: none
- Full verdict table: `dial9-viewer/ui/parity/out/walk-features01.md` (+
  .json). parity/out/ is gitignored; regenerate with the command above.

DoD "census differ legacy-vs-legacy ZERO diff":

- Same-URL diff on all three pages (index.html,
  viewer.html?trace=demo-trace.bin, flamegraph.html?trace=demo-trace.bin):
  "ZERO DIFF", exit 0 each.
- Census dump of the legacy browser page: exactly 33 affordances -
  reproduces features/01 Live-validation's recorded "33 live DOM
  affordances" count.
- Negative control: index vs viewer -> 70 diff entries, exit 1.

DoD "behavioral differ legacy-vs-legacy ZERO diff":

- `node parity/behavior-diff.mjs --a http://localhost:3021 --b http://localhost:3021`
  (all 8 journeys, 14 checkpoints, independent contexts per run):
  "ZERO DIFF", exit 0. Re-run after the steps.mjs fixes: identical.
- Negative control: extra query param on B -> 1 url.query field diff, exit 1.

DoD "axe scan produces the violation list format used in 04-ux-findings":

- 04-ux-findings has no literal violation table; its axe evidence shape is
  impact + rule + node count per page. The scan emits exactly that
  (| Impact | Rule | Description | Nodes | Example target |) plus a contrast
  summary line.
- Legacy index page: 4 violations, 28 nodes - "serious contrast violations
  (15 nodes)" reproduces finding F6's browser-page evidence verbatim;
  critical label violations per F1. Viewer: contrast 8 nodes; flamegraph:
  contrast 2 nodes (F6: "all three pages" - confirmed).
- `--fail-on serious` on legacy index exits 1 (gate mode works).

Layer (e) baseline runs (all exit 0):

- viewer (journey J1 -> storm): 176 layouts, all forced, ~1.2/frame;
  0 long tasks in storm window; render/frame "unavailable" via stub source.
- index (J6): 28 layouts, 4 forced. flamegraph (J5): 16 layouts, 4 forced.

Repo gates:

- `npx tsc --noEmit` clean (tsconfig includes src/tests only; parity/ is
  plain JS by design).
- `npm run test`: 8 files, 122 tests passed.
- `npm run build`: dist listing UNCHANGED vs pre-work snapshot (21 lines;
  parity/ never enters dist).
- `cargo build -p dial9-viewer` OK (embed check). No .rs files touched, no
  trace-format changes -> full Rust suite/stress not required per AGENTS.md.
- Dev-server and all Chromium instances killed at session end.

## REMAINING

Nothing within T12's scope. Follow-ups owned by OTHER tickets:

- CI wiring ("headless in CI, gated to UI-touching PRs" appears in T12's
  Work text): NOT landed here - the DoD contains no CI item and every DoD
  check is local-only; the differs need an old-vs-new page pair that first
  exists at T13. Flagged as a PR-review question rather than guessed at.
- Chunk 2: wire the `store` render source (devRenderAssertStats()).
- T13/T14+: walker registries for features/03 and features/02; old-vs-new
  census/behavior runs begin there (T13).

## BLOCKERS

None.
