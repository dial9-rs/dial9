# Migration tickets - chunk 1 (foundation, concept-independent)

Tickets for the viewer UI migration, drafted for cold-start implementation by
independent agents. Master plan: `docs/adr/0004-viewer-ui-migration.md` (read
its doc-index table first). Every ticket is one landable PR. NOT yet posted to
GitHub - review pending.

Conventions for every ticket (not repeated below):
- Repo rules (AGENTS.md) apply: run `cargo fmt --check`/clippy for Rust
  changes; JS/TS-only changes need the affected tests + a `cargo build -p
  dial9-viewer` embed check; never touch the frozen core (`decode.js`,
  `trace_parser.js`, `trace_analysis.js`, `format.js`, `heatmap.js`,
  `prefix_detect.js`, `creds.js`, `panel_layout.js`, `flamegraph.js`,
  `flamegraph_export.js` at `ui/` root) unless the ticket says otherwise.
- "Inventory" = `docs/ui-inventory/features/01-index-html.md`,
  `02-viewer-html.md`, `03-flamegraph-html.md`. "Lose nothing" = every owned
  row observably intact, checked via the parity tooling (T12).
- Ticket IDs here are draft-local; GH issue numbers assigned at posting.

Shared decisions (added after the cold-start verification pass; apply to ALL
tickets in all chunks - see `verifier-findings.md` for the raw findings):

- **New-UI routing convention:** `?ui=new` on the canonical page URL. T38
  implements it; T13/T14/T41 consume it as given.
- **Budget constants (2.8/N19 defaults, tunable):** resident raw window
  128 MB; raw-gzip byte cache 256 MB; eviction triggers at 90% of each budget
  (hysteresis margin).
- **Row-walker verdict mapping (T12):** the tool emits VERIFIED / FAILED /
  NOT-TRIGGERABLE. Against recorded 7-value tables: recorded VERIFIED and
  DEAD-CONFIRMED rows must re-derive VERIFIED (a DEAD row's access path
  producing no effect IS its expected behavior); recorded PARTIAL /
  NOT-TESTED / CODE-ONLY / NOT-TRIGGERABLE rows map to NOT-TRIGGERABLE and
  are listed, not gated. "Green" = zero FAILED.
- **Ledger:** `docs/tickets/ledger.md`, one line per entry:
  `<feature-row-id> | retired|amended|added | <ticket> | <reason>`.
  Maintainer sign-off = approving the PR that edits the ledger line.
- **DoD notation:** every DoD item is prefixed `check:` (mechanically
  verifiable command/assertion) or `review:` (human PR-review checklist
  item). Existing unprefixed items: treat behavioral/test items as check:,
  judgment items (e.g. "assertions preserved") as review:.
- **Test-suite reality:** `scripts/e2e-trace-tests.sh` registers 15 suites;
  the other `test_*.js` files run in NO CI today (local-only). Migrating an
  unregistered suite ADDS CI coverage; its baseline is a local `node` run,
  not prior CI green. The stress workflow invokes `test_trace_integrity.js`
  and `test_task_lifecycle.js` with NO file argument (it regenerates
  demo-trace.bin in place); the argv form appears only in repro comments.
- **Aggregation on the dev-server:** `aggregation_enabled` reads true when
  BYO creds are allowed (config.rs:30), but the dev-server never calls
  `.with_agg(...)`; populated `/api/tokio-stats` / `/api/flamegraph`
  responses are demand-driven and may 404. Any ticket needing populated
  aggregate data (T18, T40, T41) includes a work item: establish and
  document the recipe (extend dev_server.rs with an agg context, or run
  `dial9 serve` against a bucket with folded data).
- **Core-suite test colocation:** migrated tests for frozen-core files live
  under `ui/tests/core/` (core files stay at `ui/` root; `src/` colocation
  applies only to `src/` modules).

Ownership summary (traceability, chunk 1):
- features/01 (index page): T14 (all rows) + T15 (amended rows G8, C6, I2, F10-axis)
- features/03 (flamegraph page): T13 (all rows)
- features/02 (viewer page): chunk 2 (layout decided 2026-07-07); T16-T20 own
  cross-cutting mechanisms only, not page rows
- Unowned rows after chunk 1 = features/02 A-W: expected, tracked in chunk 2

---

## T01 - Refresh feature inventories against HEAD

**Goal:** The inventories were snapshotted ~2026-06-26; 9 UI-touching commits
have landed since. Bring `docs/ui-inventory/features/01..03-*.md` back to
truth so every later parity gate checks against reality.

**Context:** `docs/ui-inventory/features/*.md` (format: see each file's "How to
read" section); drift set = ALL commits returned by `git log
--since=2026-06-26 -- dial9-viewer/ui dial9-viewer/src/server` (9 at ticket
writing; re-run at implementation time) - highlights: #570 (Tokio Stats view
+ aggregation UI), #585 (landing-page URL state), #596 (group-by-runtime),
#595/#600 (multi-trace load pipelining), #582 (browse truncation), #587
(XSS), #597/#607 (creds/region), #586. Note: the inventories self-report
validation on 2026-06-30 yet miss commits landed before that date - trust
the git log, not the recorded dates.

**Owns:** inventory correctness itself (no feature rows).

**Work:** (a) for each drift commit, read its UI diff; add/update inventory
rows in the established table format with real `file:line` anchors; mark new
`CONDITIONAL` features (e.g. Tokio Stats requires `aggregation_enabled` -
see the shared-decisions aggregation note for how that flag behaves on the
dev-server). (b) Re-verify LINE ANCHORS of existing rows against HEAD - they
have drifted (verified examples: features/01 I2 parseKey cited :794-847,
now :1006; I3 :1457->1686; I4 :1482->1713). (c) Also inventory the new
`test_*.js` files added since the snapshot (five+ registered in
`e2e-trace-tests.sh` today) so T10/T11's suite counts are grounded. (d)
Update ownership summaries if new rows change ticket scopes.

**DoD:** check: every commit in the git-log drift set has either new/updated
rows or a one-line "no UI-visible change" disposition note in the PR
description; check: each new row's access path walked against the running
dev-server (`PORT=3001 cargo run -p dial9-viewer --bin dev-server --features
dev-server`), CONDITIONAL rows walked when their condition is satisfiable
per the aggregation note, else marked NOT-TRIGGERABLE with the reason;
check: spot-verified anchors (the three named above) corrected; review:
anchor refresh coverage. Snapshot date noted in each touched file.

**Out of scope:** any code change; re-validating unchanged row BEHAVIOR
(anchors only).

**Deps:** none. Blocks: T09, T12, T13, T14.

---

## T02 - Vite MPA scaffolding + dist-only embed

**Goal:** Make `dial9-viewer/ui/` a self-contained Vite multi-page project and
narrow the rust-embed folder from `ui/` to `ui/dist/`, per ADR section 3.

**Context:** ADR-0004 section 3 (tree + packaging rules);
`docs/ui-inventory/01-technical-constraints.md` H1/H2/H5 (why `build.rs` must
never invoke npm, why the frozen core stays at `ui/` root);
`docs/ui-inventory/02-architecture.md` section 2.1 (directory layout,
static-copy migration list) and section 3 (build pipeline).

**Owns:** no feature rows (infrastructure). MUST keep all three legacy pages
byte-identical in served behavior.

**Work:** `package.json` + lockfile + `vite.config.ts` + `tsconfig.json`
(strict, `noUncheckedIndexedAccess`, erasable-syntax-only) under `ui/`;
`src/` skeleton dirs; `dist/.gitkeep` committed, `dist/*` + `node_modules/`
gitignored; rust-embed `#[folder]` -> `ui/dist/` (`src/server/mod.rs:49`);
asset mechanism split: `demo-trace.bin` + `flamegraph.css` MOVE to `public/`
(Vite's verbatim-copy dir, no plugin needed); legacy pages + core files ship
into `dist/` via `vite-plugin-static-copy` (dev dependency, justified: the
temporary migration aid per architecture 2.1, list shrinks to empty as pages
migrate); MPA entries not yet migrated (thin-shell conversion happens per
page ticket). Legacy pages reference `flamegraph.css`/`demo-trace.bin` by
root-relative path - verify the served paths stay identical after the
public/ move.

**DoD:** check: `npm ci && npm run build` produces a `dist/` from which the
server serves all FOUR pages (incl. tokio_stats.html) byte-identical to
today - self-contained check: `curl` each page + referenced asset from a
server on `ui/` vs on `ui/dist/`, diff the bytes (T12 tooling does not exist
yet; do not depend on it); check: cargo-only checkout compiles with empty
UI; check: `cargo build -p dial9-viewer` then confirm the binary contains no
`test_*.js` (strings/embed listing).

**Out of scope:** CI wiring (T03), dev loops (T04), any page conversion.

**Deps:** none. Blocks: nearly everything.

---

## T03 - CI + release pipeline build stage

**Goal:** CI runs the JS toolchain; releases ship built UI, per ADR section 3
and constraints H1.

**Context:** ADR-0004 section 3; `02-architecture.md` section 3;
`.github/workflows/ci.yml` (see `trace-integrity` job for Node setup
precedent; `ci-pass` needs list at :318); `release.yml` - NOTE: publishing
goes through release-plz (no literal `cargo publish` step) and binary builds
are a separate matrix job; the UI build must precede BOTH paths.
`release-pr.yml` only opens release PRs - no build stage needed there.

**Owns:** none (infrastructure).

**Work:** ONE CI job (`ui`) running `tsc --noEmit`, `vitest run`
(`--passWithNoTests` until T10 lands real suites), and `npm run build`;
add `ui` to the `ci-pass` needs list. Release: insert `npm ci && npm run
build` before the release-plz step in `release.yml` AND at the start of the
binary-build matrix job. Lockfile-pinned installs (`npm ci`, never
`npm install`) everywhere.

**DoD:** check: CI green on a PR touching only `ui/src`, with the `ui` job
in `ci-pass` needs; check: `cargo package -p dial9-viewer --allow-dirty`
after `npm run build` produces a crate archive containing `ui/dist` with
built assets (the mechanical stand-in for a release dry-run); check:
`dial9-viewer/ui/README.md` gains the "UI development requires Node" note.

**Out of scope:** test migration itself (T10/T11).

**Deps:** T02.

---

## T04 - Dev loops (HMR + single-server)

**Goal:** Preserve the edit-refresh loop (constraint H5) and add HMR, per ADR
section 3.

**Context:** `02-architecture.md` section 3 (two modes); `with_dev_ui_dir`:
defined `src/server/mod.rs:128`, called `src/lib.rs:244` and
`src/bin/dev_server.rs:103`; the dev-server launch command incl. feature
flag is in the shared-decisions aggregation note / features/01 header.
For the HMR-loop DoD demo, use any `src/` skeleton module from T02 (a
trivial probe module is fine - no page code exists yet by design).

**Owns:** none.

**Work:** `npm run dev` = Vite dev server, `server.proxy` for `/api/*` ->
:3001 dev-server; `npm run dev:embedded` = `vite build --watch` into `ui/dist`
with `with_dev_ui_dir` repointed at `ui/dist`; delete `dial9-viewer/serve.py`
(obsolete: static-only, port conflict trap - it is not part of any documented
workflow); update `ui/README.md` dev instructions.

**DoD:** both loops demonstrated: edit a legacy page file -> refresh shows it
(embedded mode); edit a `src/` module -> HMR applies it (proxy mode); `/api/*`
works in both.

**Out of scope:** any page code.

**Deps:** T02.

---

## T05 - Type declarations for the frozen core

**Goal:** Hand-written `.d.ts` for the frozen core so strict TS can consume it
(ADR section 2; architecture 2.6/2.7). No core file changes.

**Context:** `02-architecture.md` 2.6 + 2.7; core files at `ui/` root; the
dial9-trace-loading and dial9-trace-analysis skills
(`dial9-viewer/skills/*/SKILL.md`) document the ParsedTrace schema and
analysis API - use them as the source of truth for shapes, verified against
the code.

**Owns:** none (enables everything typed).

**Work:** `src/types/` gains `.d.ts` for `decode.js`, `trace_parser.js`,
`trace_analysis.js`, `format.js`, `heatmap.js`, `prefix_detect.js`,
`creds.js`, `panel_layout.js`, `flamegraph.js`, `flamegraph_export.js` -
public API only, exactness over completeness (model what callers use; mark
genuinely dynamic parts `unknown`, never `any` without an inline justification
comment). ADR-0002 block-in-place-gap nullability encoded explicitly.

**DoD:** `tsc --noEmit` passes with a probe module importing every declared
API; declarations reviewed against at least one real call site per module
(the inline HTML pages are the call-site catalog).

**Out of scope:** `types/trace.d.ts` app-level shapes beyond what the core
returns (T06 refines); editing core files.

**Deps:** T02.

---

## T06 - App-level trace + state types

**Goal:** `types/trace.d.ts` + `types/state.d.ts`: the typed vocabulary every
module shares (architecture 2.6).

**Context:** `02-architecture.md` 2.2 (store slices) + 2.6 (typing strategy);
T05 declarations as the base; `docs/adr/0002-block-in-place-gap-is-unknowable
.md` for gap semantics.

**Owns:** none.

**Work:** kind-discriminated unions for events/panels; store slice shapes
(`trace`, `viewport`, `selection`, `uiPrefs`, `transient`, `segments` - the
last per architecture 2.8); layout geometry types wrapping `panel_layout`
outputs.

**DoD:** `tsc --noEmit` green; exhaustive-switch probe compiles (adding a
variant breaks the build - demonstrated in a test).

**Deps:** T05.

---

## T07 - Store: typed slices + RAF scheduler

**Goal:** The state model that replaces the ~68 globals and enforces the
"no render outside the scheduler" perf rule (architecture 2.2; perf rules in
ADR section 5).

**Context:** `02-architecture.md` 2.2 (mechanics: `update`/`subscribe`,
slice-set subscriptions, one RAF tick, <200 lines target);
`03-performance-findings.md` F2 (why: 32 of 35 render call sites bypass
coalescing today) and F5 (derived-data caches keyed by slices).

**Owns:** none directly (every chunk-2 ticket builds on it).

**Work:** `src/store/`: plain-object store, `update(slice, patch)`,
`subscribe(sliceSet, fn)`, RAF-coalesced notification (each subscriber at most
once per frame), a `derived(deps, compute)` cache helper invalidated by slice
change, and a dev-build assertion hook (N18) that flags renders triggered
outside the scheduler.

**DoD:** Vitest unit tests: slice isolation, one-notification-per-frame
(fake RAF), derived-cache invalidation, transient channel bypassing full
notification; 100% of store branches covered (it is small and load-bearing).

**Out of scope:** wiring any real page to it.

**Deps:** T06. Vitest available (T10) - coordinate ordering or land tests
with plain node + migrate.

---

## T08 - lib/canvas: shared drawing utilities

**Goal:** The pixel-bounding perf contracts as named, tested utilities
(architecture 2.3; ADR section 5 rule 1).

**Context:** `03-performance-findings.md` F1 (stroke() = 76% of pan CPU; the
fix direction: per-column vertices, batched same-style strokes, hoisted dash
state); `02-architecture.md` 2.3; frozen-core implementations re-exported
through the T05 types (`pixelDownsampleSpans`, `makeBarCoalescer`, palettes).

**Owns:** none.

**Work:** `src/lib/canvas/`: DPR-aware canvas wrapper (resize only on
geometry change - F3), layout.ts wrapping `panel_layout.js`, downsampling +
coalescer re-exports, a stroke-batching path builder (one path per style, one
vertex per pixel column) for series lines, palette utilities.

**DoD:** Vitest: layout invariant (label gutter + drawW + scrollbar math)
against known cases; stroke batcher emits <= width vertices for arbitrary
series; DPR wrapper no-ops when geometry unchanged.

**Deps:** T05, T06.

---

## T09 - lib/trace: typed boundary around the frozen core

**Goal:** The single import seam (architecture 2.7): `load/reparse/query/
analysis/keys/title` + barrel.

**Context:** `02-architecture.md` 2.7 (file-by-file contents + the rule that
ONLY this dir imports the core); inventory rows it must serve: features/01
I2-I4 (key parsing, title params, object URLs), features/02 B (load paths).

**Owns:** mechanism only; page rows stay with page tickets.

**Work:** per 2.7's list. `keys.ts` implements the `layout: 'known' |
'unknown'` discriminant (defect fix, ADR section 1) - port `parseKey` logic
out of `index.html`, do not import it. ESLint (or a grep-based CI check) rule:
no `src/` import of core files outside `lib/trace` + `lib/canvas` re-exports.

**DoD:** check: Vitest: `keys.ts` against the documented key layouts
(features/01 I2: #225 layout, legacy layout, positional fallback,
unknown-layout discriminant - the dev-server's 6-segment demo key from
features/01 "Live validation" Finding 1 must yield `unknown`, not shifted
fields); check: `title.ts` against features/01 I3 single- and multi-host
cases; check: `load.ts` fetch+gunzip+concat against the existing
`test_fetch_traces.js` fixtures pattern.

**Out of scope:** segment windowing (T17), worker execution (T16).

**Deps:** T01 (refreshes the features/01 line anchors this ticket ports
from), T05, T06.

---

## T10 - Vitest infrastructure + first migration batch

**Goal:** Vitest as the single runner (ADR section 7): infra + migrate the
first ~8 test files to prove the pattern.

**Context:** ADR section 7 (mechanics + dual-runner transition);
`02-architecture.md` N13 + section 3; the shared-decisions test-suite
reality note: of the 8 target suites only `test_creds.js` and
`test_prefix_detection.js` are registered in `e2e-trace-tests.sh`; the other
6 run in NO CI today (baseline = local `node` run). Assertion styles vary:
only `test_creds.js` uses `test_harness.js`; the rest use ad-hoc pass/fail
counters - the rewrite target is `describe`/`expect` regardless of source
style. Constraints H2 (core source files stay the canonical form - tests
import them via CJS interop unchanged).

**Owns:** none.

**Work:** vitest config in `vite.config.ts` (node env default); migrate the
small pure-logic suites first (`test_format.js`, `test_parse_key.js`,
`test_heatmap.js`, `test_panel_layout.js`, `test_prefix_detection.js`,
`test_creds.js`, `test_poll_color.js`, `test_time_range.js`) into
`ui/tests/core/` (shared decision: core-suite colocation); `test_parse_key`
keeps its extract-from-index.html approach for now (it tests legacy inline
code; T15 re-points it at `lib/trace/keys.ts` when the port lands); delete
migrated originals in the same PR, plus the `e2e-trace-tests.sh` lines for
the two that have them; keep un-migrated files running via the script
(dual-runner CI).

**DoD:** check: `vitest run` green with all 8 suites migrated; check: each
original `test_*.js` deleted in the same PR; review: per-suite assertion
diff - every original assertion preserved or consciously strengthened, none
dropped; check: `e2e-trace-tests.sh` still green for its remaining
registered suites (needs the local DDB environment per that script's
header); check: CI runs both runners.

**Deps:** T02, T03.

---

## T11 - Test migration completion

**Goal:** Migrate the remaining `test_*.js` suites; retire the dual-runner
setup.

**Context:** same as T10. Stress-workflow reality (shared-decisions note):
`stress-test.yml` invokes `test_trace_integrity.js` and
`test_task_lifecycle.js` via bare `node` with NO file argument -
`test_trace_integrity.js` reads the regenerated demo-trace.bin in place;
the `<file>` form exists only for offline repro. The ADR's argv->env-var
line applies ONLY to the integrity test's optional file override.

**Owns:** none.

**Work:** remaining set = every `test_*.js` under `dial9-viewer/ui/` not
migrated by T10 (enumerate at implementation time; includes the post-
snapshot additions T01 catalogs), plus retiring `test_harness.js` with its
last consumer. Suites needing generated traces/DDB stay orchestrated by
`e2e-trace-tests.sh`, which reduces to: environment setup + trace generation
+ `vitest run` (optionally filtered to the trace-dependent suites). Stress
workflow: keep its two checks as thin `vitest run --root dial9-viewer/ui -t
<suite>` invocations (or equivalent) with the integrity test's file override
moved from argv to env var (`D9_TRACE_FILE`, read when set, demo-trace.bin
default). Un-migratable suites (if any) get a line in
`docs/tickets/ledger.md` (shared-decisions format) with the reason.

**DoD:** check: zero `test_*.js` files remain under `dial9-viewer/ui/`
except ledger-listed exceptions; check: `e2e-trace-tests.sh` green in the
DDB environment; check: the stress workflow's two check steps run the
migrated suites successfully in a manually-dispatched run (link the run in
the PR); review: assertion-preservation as in T10.

**Deps:** T10.

---

## T12 - Parity gate tooling

**Goal:** Productize the verification layers (the "nothing missing"
machinery) as repeatable scripts every page ticket's DoD invokes.

**Context:** this ticket DEFINES the four verification layers (a-d below) -
the concept traces to the chunk-1 header's ownership scheme and ADR section
7's parity gate, but the layer decomposition is authored here. NOTHING to
productize exists in the repo: the audit-phase scripts (journey driver, axe
scan) lived in an ephemeral session scratchpad and are gone; features/01's
"Reproduce" block and 04-ux-findings' "Method" describe the approach and
expected output shape only. Build from scratch against those descriptions.
features/01's "Live validation results" is the verdict-table format to
emulate; the shared-decisions verdict-mapping note defines how this tool's 3
verdicts reconcile with that table's 7.

**Owns:** none.

**Work:** `ui/parity/` (dev-only, not embedded): (a) row-walker - given an
inventory file + page URL, drive each row's access path, emit VERIFIED /
FAILED / NOT-TRIGGERABLE per the shared verdict mapping; journey scripts
J1-J8 (defined as concrete step lists here, from 04-ux-findings' journey
labels) are part of this deliverable; (b) affordance census dump + differ
(two page URLs in, diff out); (c) behavioral differ: same journey on two
URLs, field-level exact diff of data readouts - define the readout schema
(event/worker/span/POI counts, task-detail numbers) as a fixture this tool
owns; (d) axe + contrast scan; (e) perf probe - drives a scripted interaction
storm (hover/pan/wheel from the journey scripts) while recording render
invocations per frame (via the store scheduler's dev hook), forced-layout
counts, and long tasks; chunk-2 DoDs assert against its output. Playwright
as a devDependency; headless in CI (gated to UI-touching PRs) and locally.

**DoD:** check: row-walker on features/01 (post-T01) against the legacy
browser page yields zero FAILED, with recorded-VERIFIED and DEAD-CONFIRMED
rows re-deriving VERIFIED (shared mapping); check: census differ and
behavioral differ, run legacy-vs-legacy on the same page, emit ZERO diff
(self-test of the comparators; old-vs-new runs begin with T13); check: axe
scan produces the violation list format used in 04-ux-findings; check:
invocations documented in `ui/README.md`.

**Out of scope:** synthetic multi-host trace generation (T42).

**Deps:** T01, T02.

---

## T13 - Migrate flamegraph.html (pipeline proof)

**Goal:** First page on the new stack, end to end: typed entry, bundle,
parity. Smallest surface (features/03: 165 rows).

**Context:** inventory `features/03-flamegraph-html.md` (the contract - every
section A-O); `02-architecture.md` 2.1/2.4 (component shapes) + section 5
(migration order rationale); ADR section 8 (staged rollout: legacy stays
default); the page's modules `flamegraph.js`/`flamegraph_export.js` are
frozen core - the page shell around them is what migrates.

**Owns:** features/03 ALL rows (A1..O-last).

**Work:** `src/pages/flamegraph/` + components; convert `flamegraph.html` to
a Vite entry served under the new-UI path (T38 routing); legacy page STAYS
servable at the canonical URL with the T38 switch injected; both carry the
always-visible switch; keep URL contract (N10: `trace=`, zoom-state params)
identical in the new version.

**DoD:** T12 row-walker green on all features/03 rows against the new page;
affordance census diff old-vs-new empty (or ledger-justified: +switch
control); behavioral differ: identical sample counts/tree for the demo
trace; switch round-trip keeps the trace loaded; existing flamegraph export
tests green; bundle within N6.

**Out of scope:** UX amendments (keyboard unification lands in T20; the
severed-from-time finding S7 is a chunk-2 concept decision); legacy page
removal (T39).

**Deps:** T02, T05-T09, T12, T38.

---

## T14 - Migrate index.html (browser page)

**Goal:** Second page: the S3 browser, behavior-preserving port (amendments
split to T15).

**Context:** inventory `features/01-index-html.md` incl. its Live-validation
section (the page was validated row-by-row - reproduce those verdicts);
`heatmap.js`/`prefix_detect.js`/`creds.js` are frozen core; T01 will have
added the Tokio Stats + URL-state rows (#570/#585) - they are owned here too.

**Owns:** features/01 ALL rows (A-I + T01 additions), EXCEPT the four
amendment targets listed in T15 which are ported as-is here and amended there.

**Work:** `src/pages/browser/` + components (search controls, heatmap chrome,
raw table, creds panel, actions bar per 2.4's component list); canvas heatmap
render via `lib/canvas`; state in the store; URL-state behavior (#585)
preserved; served under the new-UI path (T38), legacy stays at the canonical
URL, both carry the always-visible switch.

**DoD:** T12 full-layer pass: row-walker on features/01 green (matching or
exceeding the recorded validation verdicts, NOT-TRIGGERABLE rows explicitly
listed); census diff empty (+switch control, ledger); behavioral differ
green on J6 journey; switch round-trip keeps bucket/search context (query
string preserved); existing creds/heatmap/prefix tests untouched and green.

**Out of scope:** T15's amendments; visual redesign; legacy removal (T39).

**Deps:** T02, T05-T09, T12, T38; T01 (row additions).

---

## T15 - Index page contract amendments (4 defect fixes)

**Goal:** The four structurally-fixed defects on the browser page (ADR
section 1), landed AFTER the faithful port so parity and amendment are never
in one diff.

**Context:** ADR section 1; findings: features/01 G8 (dead column sort) +
Live-validation Findings 1-3 (parseKey mislabel -> `unknown` layout handling
in UI, dial9 bucket-filter lockout -> config-driven predicate, time-only
heatmap axis -> date+time on day-crossing spans); `04-ux-findings.md` rows F3
(legend coverage) is NOT included - chunk 2.

**Owns:** amended behavior of features/01 rows G8, C6, I2(+display), F10-axis
analog (heatmap axis, features/01 F10).

**Work:** sortable raw-results table (sorting = component capability);
unknown-layout keys render raw (no shifted columns) via T09's discriminant;
bucket filter becomes server-config/query-param predicate with current
behavior as default; heatmap axis renders date+time when the span crosses a
day boundary.

**DoD:** check: per-fix Vitest coverage; check: row-walker re-run on the
amended rows with UPDATED inventory rows in the same PR (contract amended
consciously - the inventory change is part of the diff); check: ledger
entries in `docs/tickets/ledger.md` (shared format). Bucket-filter predicate
decision: a `bucket_filter` field in `/api/config` (server-side default
"dial9"), overridable by a `bucket_filter=` query param; T15 re-points
`test_parse_key` at `lib/trace/keys.ts` (closing T10's interim note). Sort
semantics: all raw-table columns sortable; numeric for epoch/size/seg#,
lexical otherwise.

**Deps:** T14, T12 (row-walker), T09 (keys.ts).

---

## T16 - Web Worker load pipeline

**Goal:** Parse off the main thread (ADR section 6 "do now"; architecture
section 2.8 sequencing): kills the 3.8-12.2s load stall without touching the
core.

**Context:** `03-performance-findings.md` (load walls, measured);
`02-architecture.md` 2.7 (`load.ts`) + 2.8 (worker placement); the frozen
core is environment-agnostic (runs under Node) - it runs in a Worker
unchanged; streaming progress contract: features/02 B (load-timing UX rows).

**Owns:** load mechanism; viewer page rows stay in chunk 2.

**Work:** worker entry bundling the core (Vite worker build); `load.ts`
orchestrates: fetch runs INSIDE the worker (single AbortController wiring;
decided here), gunzip+parse in worker, transfer the result to the store's
`trace` slice. FIRST work item: verify ParsedTrace is structured-cloneable
(scan for functions/getters/class instances in the parse output; the
features/01 `parseKey` getters are page code, not core output - but VERIFY
the core's output); if not cloneable, define a serializable snapshot at the
worker boundary and document it in `types/trace.d.ts`. Progress messages
carry the load-timing fields the current UX shows (features/02 B rows list
them). Note 2.8 describes the worker in the per-segment tier; this ticket
builds the whole-trace path that T17 then window-izes.

**DoD:** check: Vitest integration test parsing the demo trace via the
worker path with output deep-equal to direct parse (counts + spot samples) -
worker tests run under Vitest browser mode OR a Node `worker_threads` shim
with a Worker-API adapter (pick one, document it; node env alone has no Web
Worker); check: progress events received with the load-timing fields
non-empty; check: abort mid-parse -> worker terminated (no live handle),
store `trace` slice unchanged, no pending progress callbacks fire after
abort (assert on those three named observables).

**Out of scope:** segment windowing (T17); any UI.

**Deps:** T05, T06, T09.

---

## T17 - Segment-windowed loading (tier 2 core)

**Goal:** The 2.8 detail tier: `segments` store slice, viewport-driven fetch,
+/-1 boundary prefetch, LRU eviction, two-level cache, budget.

**Context:** `02-architecture.md` 2.8 ENTIRELY (three verified properties,
tier 2 mechanics, budget N19, hard edges - the FOUR client-side ones are DoD
items here; the aggregate-coverage edge belongs to T18);
`lib/trace/segments.ts` placement per 2.8. Constants: shared-decisions
budget block (128 MB resident raw / 256 MB gzip cache / evict at 90%).
Segment key = the S3 object key string (or the `trace=` component URL for
non-S3 sources); per-segment time extents parse from the key path + listing
`last_modified`, exactly as the browser page's heatmap derives them
(features/01 F2 rows).

**Owns:** mechanism + N19. Viewer integration = chunk 2.

**Work:** segment state machine (`listed -> fetching -> parsed -> evicted`);
need-set/prefetch/eviction logic as pure, Vitest-testable functions;
raw-gzip-bytes cache with its own budget; stale-fetch abort; boundary-poll
truncation markers surfaced in the parsed output (extend open-ended-poll
semantics to window edges - features/02 G5 precedent); budget hysteresis
per the shared constants.

**DoD:** check: Vitest per hard edge - abort on viewport jump; boundary
poll marked truncated, not long; re-entry hits raw cache not network (mock
fetch call counts); eviction fires at the 90% threshold and resident stays
under budget; check: multi-segment scenario built from repeated-demo-trace
fixtures (sufficient here; real encoder fixtures arrive with T42) navigates
a simulated 10-segment trace end-to-end with resident raw <= 128 MB
throughout.

**Out of scope:** minimap/UI; aggregates + coverage fallback (T18).

**Deps:** T07, T09, T16.

---

## T18 - Tier-1 aggregates client

**Goal:** Typed client for the existing server aggregate endpoints - the
zero-raw-bytes overview source (2.8 tier 1).

**Context:** `02-architecture.md` 2.8 tier 1 + coverage hard edge; server
code: `dial9-viewer/src/server/tokio_stats.rs`, `flamegraph.rs` (request/
response shapes incl. `refine=1` progressive polling and `Coverage`).
Error-shape reality: unavailable aggregation returns 404 (tokio_stats.rs:81,
flamegraph.rs:204), internal errors 500 - there is NO 503. The
`aggregation_enabled` config flag is `state.agg.is_some() ||
state.allow_byo_creds` (config.rs:30), so it can be true while the endpoints
still 404 - the client must treat 404 as "no data", not as an error.
`Coverage` exposes counts only (files/hosts matched vs folded) - "partial" =
folded < matched, computed client-side.

**Owns:** mechanism + the coverage-fallback hard edge (client side: expose
a `coverage: full | partial | none` signal consumers use to fall back to
listing metadata); consuming surfaces (minimap etc.) = chunk 2.

**Work:** `lib/trace/aggregates.ts`: typed requests/responses for both
endpoints; refine-polling helper (poll with `refine=1` until the response's
folded counts stop increasing or a poll ceiling hits - document the chosen
termination rule); coverage signal; degradation: 404 -> `none`,
flag-false -> `none` without a request, partial counts -> `partial`. Work
item: establish + document the populated-fixture recipe per the
shared-decisions aggregation note (extend dev_server.rs with an agg context,
or capture from `dial9 serve` against folded data), then record fixtures.

**DoD:** check: Vitest against the recorded fixtures for both endpoints
incl. a refine sequence; check: degradation paths - 404, flag-false,
partial-coverage counts - each yields the documented signal without
throwing; check: the fixture recipe is documented and reproducible.

**Deps:** T05, T06.

---

## T19 - Viewer URL view-state + copy-link

**Goal:** UX priority S3: the viewer URL carries viewport/selection/POI/time
mode; refresh and share stop destroying analysis. (Landing page already does
this - #585 - the viewer does not.)

**Context:** `04-ux-findings.md` S3 (evidence: URL identical after zoom,
measured); ADR section 9; URL contract stability = `02-architecture.md` NFR
N10 (NOT an inventory row id - the architecture doc's N10; additive only,
old links must still work). Time mode is included in the state set as a
design addition beyond S3's viewport/selection/POI (rationale: restoring a
shared view with the wrong clock mode changes what the reader sees). Store
slices (T07 mechanism; the slice SHAPES this codec serializes are defined
here for what exists at landing time, extended by chunk-2 tickets as viewer
slices appear - the codec schema is versioned exactly for that).

**Owns:** mechanism + the S3 finding. (Viewer chrome that displays "copy
link" lands with chunk-2 status bar; a minimal toolbar button ships here so
the feature is usable on migrated pages.)

**Work:** URL hash codec (versioned schema) <-> store slices; debounced
history.replaceState on slice change; restore on load; copy-link action.
First work item: enumerate the flamegraph page's EXISTING zoom-state URL
params by reading `flamegraph.js`/features/03 section M (they are not listed
in any linked doc) and record them as the codec's legacy-param test fixture;
unify through the codec without breaking them. Integration at landing time =
the pages then migrated (flamegraph via T13; browser page state via #585
already has its own URL sync - reconcile, do not duplicate).

**DoD:** check: Vitest codec round-trip property test; check:
restore-on-load integration test on each page migrated at landing time
(enumerate in the PR; flamegraph at minimum); check: the recorded
legacy-param fixture URLs still resolve identically (T12 behavioral differ
- direct dependency); review: schema documented for chunk-2 extension.

**Deps:** T07, T12 (differ + fixtures), T13 (first consuming page).

---

## T20 - Unified keyboard model

**Goal:** UX priorities K1-K3: one vocabulary on all three pages - `/` search
palette, `n`/`p` POI step, `g` goto-time, `f` fit, `z` zoom-undo, WASD nav,
`?` help everywhere; existing bindings unchanged.

**Context:** `04-ux-findings.md` K1-K5 (evidence + genre-standard table); ADR
section 9 (adopted model); interactive spec: `docs/ui-inventory/mocks/
keyboard.html` (canonical key semantics: W=zoom in, S=zoom out, A=pan left,
D=pan right, palette Enter-cycling, status announcements);
`02-architecture.md` 2.5 (keys dispatch store actions, never render).

**Owns:** the keyboard MECHANISM (router, palette, zoom stack, help
component) + findings K1 (search exists), K2 (browser-page keyboard:
heatmap window selection + search submit get keyboard paths - this ticket's
browser integration), K3 (one vocabulary; `?` help on every migrated page).
K4/K5 mechanisms ship here; their viewer INTEGRATION is T23's (boundary:
T20 exposes actions + router, T23 wires viewer surfaces to them). "Existing
bindings unchanged" = no binding that works on a legacy page today is
removed or re-mapped on its migrated version; adding bindings where keys
were dead is the point, not a violation.

**Work:** `lib/interact/keyboard.ts`: focus-tolerant key router (K5), search
palette component (tasks/spans/POIs via `lib/trace/query` - T09, direct
dep), goto-time, zoom history stack (store), fit action, unified help
overlay COMPONENT. Per-page wiring at landing time: flamegraph (T13) - `?`
help (replaces its dead ?, features/03 H rows are the content baseline), `/`
search (exists - unify), `f` fit, `z` zoom-undo; browser (T14) - `?` help
(exists), `/` focuses search, heatmap keyboard window-selection per K2's fix
direction, Enter submits. Viewer keys (n/p, g, WASD on timeline) activate in
chunk 2; the palette indexes whatever `query` exposes per page.

**DoD:** check: Vitest for router + palette filtering + zoom stack; check:
row-walker on features/03 help/search rows (migrated flamegraph) and the
features/01 rows touched by browser wiring - amended rows updated in-diff +
ledger; check: `?` opens help on every page migrated at landing time
(enumerate in PR); check: axe clean on palette/overlay (T12).

**Deps:** T07, T09, T12, T13/T14 (pages to land on).

---

## Chunk 2 (pending - requires track-B concept pick)

Viewer page migration by slice: layout shell per chosen concept, panels
(spans/events/CPU/queue/task-detail) one ticket each, lanes + pointer
interactions, sidebar, minimap + status bar (consumes T17/T18), POI list
(concept 2) or equivalent, legend/tooltip amendments (04 F2/F3/F7), staged
rollout switch (ADR section 8), legacy removal + final full-inventory gate.
features/02 rows partition across these tickets; the ownership summary above
is extended then.
