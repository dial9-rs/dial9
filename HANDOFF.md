# HANDOFF - T10 (Vitest infrastructure + first migration batch)

## STATUS

DONE. Vitest infra wired (config in vite.config.ts, node env), all 8 target
suites migrated to `ui/tests/core/*.test.ts` with originals deleted in the
same commits, e2e-trace-tests.sh trimmed of the two migrated registrations,
all local gates green. The only pending-CI item is the FULL
e2e-trace-tests.sh run (its demo-trace regeneration step needs DDB Local);
all 13 remaining registered suites were run individually with plain `node`
against the checked-in demo trace and pass.

Branch: `ticket/T10-vitest-first-batch` (base 70abaab, the T04 tip).
Worktree: `/Users/facundo/code/wye/dial9-tokio-telemetry/.claude/worktrees/T10`

## COMPLETED (commit shas)

- de238d6 test(viewer): wire Vitest config into vite.config.ts
  - `test` key added to ui/vite.config.ts (NO separate vitest.config file,
    per 02-architecture.md 2.1) with `/// <reference types="vitest/config" />`;
    environment "node"; include: `tests/core/**/*.test.ts` (migrated legacy
    suites, shared decision: core-suite colocation) + `src/**/*.test.ts`
    (future TS modules).
  - tsconfig.json include extended to `tests` - the same strict flags
    (strict, noUncheckedIndexedAccess, verbatimModuleSyntax,
    erasableSyntaxOnly) apply to test code.
  - `@types/node` devDep added (tests import node:fs/node:vm/node:module;
    nothing else in the project needed it before).
- 69cfa2c migrate format, prefix_detection, poll_color (originals deleted);
  `npm test` drops `--passWithNoTests` so an accidentally-empty include
  pattern now fails loudly instead of passing silently.
- 1e59d58 migrate heatmap, panel_layout (originals deleted).
- bbbea31 migrate creds, parse_key (originals deleted). test_harness.js
  KEPT: 8 unmigrated suites still require it.
- f82c96f migrate time_range (original deleted).
- d8b63cf e2e-trace-tests.sh: removed ONLY the test_creds.js and
  test_prefix_detection.js lines (13 registered legacy suites remain);
  ui/README.md testing section documents the dual-runner state;
  design/architecture.md re-points its test_panel_layout.js reference.

## CJS interop pattern (used consistently in all 8 suites)

`createRequire(import.meta.url)` + destructure + `as`-cast to a local
minimal type. Justification: the frozen core files assign `module.exports`
inside an `if (typeof module !== "undefined")` guard, which cjs-module-lexer
cannot statically analyze, so ESM named-import interop of those files under
Vite/Vitest is unreliable. `createRequire` loads them through Node's native
CJS loader - byte-identical module-loading semantics to the legacy
`node test_*.js` baseline, and no Vite transform ever touches the frozen
files (constraint H2). The `as`-cast gives the strict TS flags real types to
check test bodies against.

Two path-dependent suites resolve files relative to the test file via
`fileURLToPath(new URL("../../<file>", import.meta.url))`:
- parse_key.test.ts reads index.html (extract-parseKey-via-regex + vm
  sandbox mechanism preserved verbatim; T15 re-points it at
  lib/trace/keys.ts when the TS port lands).
- panel_layout.test.ts reads viewer.html (CSS padding-left invariant grep).
- time_range.test.ts reads public/demo-trace.bin (T04 location preserved);
  the original argv[2] trace-path override became the DIAL9_TRACE_PATH env
  var per ADR-0004 section 7 ("trace-file-parameterized ... switch from argv
  to an env var").

## Per-suite assertion diff (review DoD item)

Counts are assertion call sites (definitions excluded). "Split" = a compound
`ok(a && b)` becoming one expect per leg - every original condition is
preserved; none dropped.

| Suite | Original | Migrated | Notes |
|---|---|---|---|
| test_format.js -> format.test.ts | 42 assertEq | 42 expect | 1:1, one it() per original assertEq, labels preserved |
| test_prefix_detection.js -> prefix_detection.test.ts | 8 assert | 8 expect | 1:1; original `=== true/false` identity preserved via toBe(true/false) |
| test_poll_color.js -> poll_color.test.ts | 7 fail sites guarding ~31 loop conditions | 9 expect statements executing 33 assertions | Same inputs/conditions; 2 compound equality chains (floor c0=c1=c2, ceiling c1=c2=c3) split into 2 expects each; loops now check every element with expect instead of early-return fail |
| test_heatmap.js -> heatmap.test.ts | 39 ok/approx | 50 expect | 11 compound `ok(a && b [&& c])` split per leg; approx(a,b,eps) preserved exactly as \|a-b\| <= eps (expectApprox helper) |
| test_panel_layout.js -> panel_layout.test.ts | 24 assert | 24 expect | 1:1 incl. the viewer.html CSS-grep invariant test; strictEqual -> toBe, ok(finite) -> toBe(true) |
| test_creds.js -> creds.test.ts | 36 assert | 37 expect | +1: `get()` result null-checked before field access (TS strictness, also a real assertion). "rejects on missing field" try/catch+2 asserts merged into one `rejects.toThrow(/required/)` (still checks threw AND message). listBuckets-error compound `/401/ && /rejected/` split into 2 matches + explicit threw check |
| test_parse_key.js -> parse_key.test.ts | 13 assertEq + 2 guards | 17 expect | 13 field checks 1:1; locate-guard -> expect not.toBeNull; compound object check (`!p \|\| typeof p !== "object"`) split into 2; +1 strengthened: extracted parseKey checked to be a function before use |
| test_time_range.js -> time_range.test.ts | 22 fail sites | 23 expect | +1: trace-file-exists guard is now an expect instead of console.error+exit. Per-event in-range loops report offender COUNT (filter + toBe(0)) instead of bailing at the first offender - strictly stronger. EVENT_TYPES import dropped (was imported but never used in the original) |

Restructuring note (applies to poll_color, heatmap, creds, parse_key,
time_range): compound conditions were split so a failure pinpoints the exact
leg; nothing was weakened and no original condition was removed. Suite/it
names keep the original labels so failures map back to the old output.

## DoD evidence

- `vitest run` green, all 8 suites migrated:
  `Test Files 8 passed (8) / Tests 122 passed (122)` (~2.5s; time_range
  dominates - 7 parses of the 3.4MB demo trace).
- Originals deleted in the same commits as their replacements (see shas
  above); `ls dial9-viewer/ui/test_*.js` no longer lists any of the 8.
- `npx tsc --noEmit`: clean (strict flags cover tests/ via tsconfig include).
- `npm run build`: dist listing IDENTICAL before/after the whole ticket
  (19 files; diff of sorted `find dist -type f` = empty). Migrated tests are
  not in dist and not in the embed set (rust-embed folder is `ui/dist/`).
- `cargo build -p dial9-viewer`: green
  (CARGO_TARGET_DIR=/Users/facundo/code/wye/dial9-tokio-telemetry/target).
  NO Rust files touched in this ticket (diff stat vs 70abaab confirms).
- e2e-trace-tests.sh: `bash -n` clean; the full script needs DDB Local for
  its regenerate-demo-trace step (per its header) which is not available in
  this environment -> full-script run is PENDING-CI (trace-integrity job).
  Compensating local evidence: all 13 REMAINING registered suites run
  individually with plain `node` against the checked-in
  ui/public/demo-trace.bin - all PASS: trace_integrity, task_lifecycle,
  trace_analysis, trace_properties, fetch_traces, stream_parse,
  parse_yield_throttle, url_state, all_skills_snippets, flamegraph_api,
  enclosing_spans, flamegraph_export, runtime_groups.
- CI runs both runners, verified with NO workflow edits needed:
  - `ui` job (.github/workflows/ci.yml:178-195): npm ci, tsc --noEmit,
    `npm run test` (= `vitest run` now), npm run build.
  - `trace-integrity` job (ci.yml:153, run at :176): scripts/e2e-trace-tests.sh.
- Frozen core, unmigrated tests, the four HTML pages: untouched (index.html
  and viewer.html are only READ by the migrated suites).

## REMAINING

- Full e2e-trace-tests.sh execution: pending CI (needs DDB Local; see above).
- 20 legacy `test_*.js` suites still unmigrated (21 files at the ui/ root
  minus test_harness.js; 13 of them registered in e2e-trace-tests.sh, 7 in
  no CI - later tickets); dual-runner CI stays until the last one moves.

## BLOCKERS

None.

## Notes for reviewers / next tickets

- ci.yml:193 comment ("--passWithNoTests until T10 migrates real suites to
  Vitest") is now stale - the flag is gone from package.json and the job
  behaves correctly unchanged. Left untouched: the ticket scoped workflow
  edits strictly to keeping both runners running. Whoever next touches
  ci.yml can drop the comment line.
- heatmap.js:21 (frozen core) still says "unit-tested in test_heatmap.js";
  frozen files are out of scope, and the comment will be resolved when the
  file is ported.
- `npm run build` locally deletes the committed `ui/dist/.gitkeep` (vite
  empties outDir; pre-existing behavior, not introduced here). Restore with
  `git checkout -- dial9-viewer/ui/dist/.gitkeep` before committing if you
  build locally.
- test_parse_key regex + T15: parse_key.test.ts carries the same brittle
  `/function parseKey\([\s\S]*?\n    \}\n/` extraction as the original; if
  index.html's inline parseKey is ever reformatted before T15 lands, the
  beforeAll locate-guard fails with a clear message.
