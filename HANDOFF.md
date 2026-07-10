# HANDOFF - T11 (Test migration completion; dual-runner retirement)

## STATUS

DONE (pending-CI items listed below). All remaining legacy `test_*.js` suites
are migrated to Vitest under `ui/tests/core/*.test.ts` except the single
ledger-listed exception (`test_parser.js`); `test_harness.js` is retired;
`e2e-trace-tests.sh` is reduced to env setup + demo-trace regeneration + a
filtered `vitest run`; the stress workflow's two checks are Vitest
invocations with the integrity test's file override moved from argv to the
`D9_TRACE_FILE` env var; AGENTS.md / ui README / vite.config comments now
describe the Vitest-only reality.

Branch: `ticket/T11-test-migration-completion` (base 98a5b3b, the T10 tip).
Worktree: `/Users/facundo/code/wye/dial9-tokio-telemetry/.claude/worktrees/T11`

## COMPLETED (commit shas)

- 197cac1 migrate task_lifecycle, enclosing_spans, url_state, runtime_groups
  (originals deleted; their 4 lines removed from e2e-trace-tests.sh).
  `findTaskLifecycleInversions` moved from test_trace_integrity.js's exports
  to `tests/core/helpers/task_lifecycle_inversions.ts` (a Vitest test file
  must not import another test file; both integrity and lifecycle suites
  import the helper). Full doc comment carried over.
- 1b1411a migrate trace_integrity, trace_properties, block_in_place,
  flamegraph_api (originals deleted; 3 registered lines removed).
  trace_integrity's argv[2] override -> `D9_TRACE_FILE` env var per the
  ticket (default `public/demo-trace.bin`).
- c86f4ad migrate fetch_traces, stream_parse, parse_yield_throttle, slice
  (originals deleted; 3 registered lines removed). Trace-sized byte /
  canonical-object comparisons stay on `node:assert deepStrictEqual` inside
  the Vitest tests - chai's deep-eql walks multi-MB arrays element-by-element
  and times out (found empirically; the originals used assert.deepStrictEqual).
- 289b593 migrate flamegraph_export, flamegraph_recipes, directory_parse,
  directory_scale; DELETE test_harness.js with its last consumer (originals
  deleted; flamegraph_export's registered line removed).
- 3118dd3 migrate trace_analysis (92 tests), all_skills_snippets,
  diagnose_setup (originals deleted).
- f754bbe retire the dual runner: e2e-trace-tests.sh rewrite (env setup +
  regeneration + filtered `vitest run` over a TRACE_SUITES list = the 13
  suites it historically ran; `npm ci` only when node_modules is absent; CLI
  contract unchanged), stress-test.yml both check steps -> vitest (+ npm ci
  step), AGENTS.md Testing section + ui/README.md Tests section rewritten,
  vite.config.ts test comment updated, stale test_*.js comment references in
  regenerate_demo_trace.sh / regenerate_block_in_place_trace.sh re-pointed,
  ci.yml's stale `--passWithNoTests` comment dropped (T10's handoff flagged
  it for the next ci.yml edit).
- 801b767 vitest global testTimeout/hookTimeout = 120s in vite.config.ts +
  stream_parse describe ceiling 300s: with all 27 test files running in
  parallel workers, demo-trace parses exceed Vitest's 5s/10s defaults
  (verified: a full-suite run produced 4 test timeouts + 1 hook timeout,
  zero assertion failures; green after the raise).
- (final commit) this HANDOFF.

## Ledger lines (shared-decisions format)

- `test_parser.js | retired | T11 | NOT migrated - kept as a plain Node script at the ui/ root: the Rust integration test dial9-tokio-telemetry/tests/js_parser.rs invokes it by filename with positional file args (trace.bin, expected.jsonl); migrating requires a .rs edit, which is out of scope/forbidden for T11. "retired" here means retired from the migration plan, not deleted.`
- `test_diagnose_setup.js | amended | T11 | Migrated to tests/core/diagnose_setup.test.ts, but the missing-trace-dir behavior changed from exit-1 to describe.skipIf (suite skips when D9_DIAGNOSTIC_TRACES / /tmp/dial9-diagnostic-traces is absent). The original exit-1 was a usage guard for a manually-invoked script; under a single always-on runner, skip is the only sane translation. Run scripts/generate_diagnostic_traces.sh to exercise it.`
- `test_flamegraph_recipes.js | amended | T11 | "Recipe polls > 5ms" carries vitest it.fails: pre-existing failure on the untouched lineage, data-dependent on the committed demo trace (pre-ruled in the T11 ticket; verified by the T04 implementer against main). All other recipe assertions genuinely pass. flamegraph_recipes.test.ts is deliberately NOT in e2e-trace-tests.sh's TRACE_SUITES: against a REGENERATED trace the recipe may legitimately find samples, which would invert it.fails into a failure.`

## Pre-ruling applications and judgment calls

- D9_TRACE_FILE (ticket-mandated) implemented for trace_integrity only;
  T10's DIAL9_TRACE_PATH for time_range left untouched. Naming inconsistency
  recorded under Open Questions.
- Other argv affordances that Vitest cannot express became env vars (not
  covered by the ADR line, which applies only to the integrity test; these
  were the minimal translation to avoid losing capabilities):
  - `D9_SCALE_LARGE=1` = directory_scale's old `--large` (200 files vs 20).
  - `D9_DIAGNOSTIC_TRACES` = diagnose_setup's old argv[2] trace dir.
  - trace_analysis's argv[2] trace-path override was DROPPED (offline-repro
    affordance never used by any script; the suite now always reads the demo
    trace). Flag if that capability should be restored via env var.
- e2e-trace-tests.sh runs a FILTERED vitest set (TRACE_SUITES = exactly the
  13 suites the script ran before) rather than the whole tests/core/ tree:
  (a) the ticket allows it, (b) the full tree runs in the `ui` job anyway,
  (c) flamegraph_recipes' it.fails must not meet a regenerated trace (see
  ledger), (d) directory_* / slice / creds etc. are demo-trace-shape-independent.
- test_harness.js deleted in 289b593 together with its last consumers
  (fetch/stream/slice/yield went in c86f4ad; export/recipes in 289b593).
- ci.yml edit is comment-only (stale `--passWithNoTests` note); no workflow
  logic changed there.

## Per-suite assertion diff (review DoD item)

Counts are assertion call sites (definitions excluded); "sites" not
executions - loops asserting per element count once, exactly as in the
originals. Per T10 precedent, first-offender/break loops now COLLECT all
offenders and assert the collection is empty (strictly stronger); it()/
describe() names keep the original labels/comments.

| Suite | Original | Migrated | Notes |
|---|---|---|---|
| test_task_lifecycle.js -> task_lifecycle.test.ts | 15 assert.strictEqual | 15 expect | 1:1; imports the moved helper |
| test_enclosing_spans.js -> enclosing_spans.test.ts | 6 deepStrictEqual | 6 expect | 1:1 |
| test_url_state.js -> url_state.test.ts | 47 strictEqual/deepStrictEqual | 47 expect | 1:1 (toBe / toStrictEqual / toBeUndefined) |
| test_runtime_groups.js -> runtime_groups.test.ts | 34 assert.* | 34 expect | 1:1; 12 numbered blocks -> 12 it()s named from their comments |
| test_trace_integrity.js -> trace_integrity.test.ts | 24 fail sites + 2 exit guards | 26 expect | guards -> beforeAll expects; testAllEventTypesPresent -> one it() per EVENT_TYPES entry; offender loops collect all violations |
| test_trace_properties.js -> trace_properties.test.ts | 23 ok/eq sites | 23 expect | 1:1 incl. the conditional golden-fixture branches (skip paths keep their console.log messages) |
| test_block_in_place.js -> block_in_place.test.ts | 25 assertEq/assert sites | 26 expect | +1: fixture-missing fail-and-return became an explicit exists expect |
| test_flamegraph_api.js -> flamegraph_api.test.ts | 46 assertEq/assertDeepEq | 46 expect | 1:1; TZ=America/New_York set at module scope exactly like the original |
| test_fetch_traces.js -> fetch_traces.test.ts | 25 assert.* | 25 expect/deepStrictEqual | 1:1; trace-sized byte equality via node:assert (expectBytesEqual) |
| test_stream_parse.js -> stream_parse.test.ts | 10 assert.* sites | 10 expect/deepStrictEqual | 1:1; canonical comparisons via node:assert; header-dependent tests registered from module-load prelude as before |
| test_parse_yield_throttle.js -> parse_yield_throttle.test.ts | 8 assert.* | 8 expect | 1:1 (2 sanity asserts moved to beforeAll) |
| test_slice.js -> slice.test.ts | 30 assert.* sites | 30 expect | per-event assert loops -> filter+count-0 expects (same conditions, all offenders reported) |
| test_flamegraph_export.js -> flamegraph_export.test.ts | 55 assert.* | 55 expect | 1:1; demo-trace test keeps its exists guard via it.skipIf |
| test_flamegraph_recipes.js -> flamegraph_recipes.test.ts | 14 assert sites + 2 exit guards | 16 expect | +1 beforeAll exists expect, +1 the bare `minTs + N` arithmetic statement now asserts Number.isFinite; ONE test it.fails (ledger) |
| test_directory_parse.js -> directory_parse.test.ts | 42 assert sites + 1 exit guard | 43 expect | 1:1; temp-dir/cache/mtime mechanisms preserved verbatim |
| test_directory_scale.js -> directory_scale.test.ts | 10 assert sites + 1 exit guard | 11 expect | 1:1; sequential it()s share the seeded dir (warm-after-cold order preserved); --large -> D9_SCALE_LARGE |
| test_trace_analysis.js -> trace_analysis.test.ts | 238 fail sites | 225 expect (92 it()s) | every original condition preserved; delta = compound merges where several fail branches checked fields of one value (single boolean expect with the original messages) and break-on-first loops -> offender collections |
| test_all_skills_snippets.js -> all_skills_snippets.test.ts | dynamic pass/fail counters | one it() per (mode, recipe) + one per skill script + 2 schema it()s | snippet mechanism (strip requires, const->assignment rewrite, new Function, ui-root require) verbatim; skipped recipes -> it.skip (visible); schema top-level/deep error lists -> toEqual([]) |
| test_diagnose_setup.js -> diagnose_setup.test.ts | 9 assert sites | 9 expect | 1:1; per-subdir exists guards -> it.skipIf; whole suite describe.skipIf when trace dir absent (ledger) |

## DoD evidence

- Zero `test_*.js` under dial9-viewer/ui/ except the ledger-listed
  `test_parser.js` (`ls dial9-viewer/ui/test_*.js`). `test_harness.js` gone.
  `bench_parse.js` untouched (benchmark, per ticket).
- `npx tsc --noEmit`: clean (strict flags cover all new tests).
- `npm run test` (full suite, 27 files): `Test Files 26 passed | 1 skipped`
  / `Tests 534 passed | 1 expected fail | 11 skipped`, exit 0. The only
  non-pass is the sanctioned `it.fails` ("Recipe polls > 5ms"); the skipped
  file is diagnose_setup (no generated diagnostic traces on this machine)
  and the 11 skipped tests are its 5 plus the 6 recipe snippets the
  original's shouldSkip() also never ran.
- e2e-trace-tests.sh: `bash -n` clean; its exact TRACE_SUITES list was run
  locally against the committed demo trace: 13 files, 358 passed, 6 skipped
  (recipe skips), 0 failures. The full script (regeneration step) needs DDB
  Local, unavailable here -> PENDING-CI (trace-integrity job).
- Stress workflow: both steps rewritten; `D9_TRACE_FILE` behavior verified
  locally (override to a copied trace: 25/25 pass; missing file: loud
  beforeAll failure, non-zero exit). Manually-dispatched run: PENDING-CI (no
  pushes allowed from this ticket).
- `npm run build`: green; dist listing = the same 19 files T10 recorded, no
  test files in dist (vite.config.ts diff vs 98a5b3b touches only comments
  and the `test:` key, which does not affect build output).
  `dist/.gitkeep` restored after the local build (pre-existing vite
  empty-outDir behavior, per T10's note).
- `cargo build -p dial9-viewer` (shared CARGO_TARGET_DIR): green.
- Zero .rs changes: `git diff 98a5b3b..HEAD --stat -- '*.rs'` empty.

## PENDING-CI

1. Full e2e-trace-tests.sh in the DDB environment (trace-integrity job).
2. Manually-dispatched stress-test.yml run exercising the two migrated
   checks (workflow_dispatch after merge; both invocations verified locally
   in equivalent form).

## Open questions / follow-ups for the orchestrator

1. Env-var naming inconsistency (pre-flagged by the ticket): the integrity
   test uses `D9_TRACE_FILE` (ticket-mandated) while T10's time_range uses
   `DIAL9_TRACE_PATH`. Both work; unify in a later ticket if desired.
2. trace_analysis's old argv trace-path override was dropped (see judgment
   calls). Restore as an env var if anyone actually used it offline.
3. T12 may touch ui/package.json (Playwright) and T06 adds ui/src/types/ -
   no conflicts expected: this ticket did not modify package.json/src at all.

## Notes for reviewers

- vitest global testTimeout/hookTimeout are now 120s (stream_parse describe:
  300s). These are straggler ceilings, not expected durations: full-suite
  parallelism multiplied per-parse times ~4x on this machine and CI runners
  are slower. Without them, default 5s/10s limits produced spurious timeout
  failures in time_range/trace_properties/stream_parse.
- The full `npm run test` takes ~2-4 minutes locally (dominated by
  stream_parse's byte-by-byte chunking and all_skills_snippets' two-mode
  snippet runs - the same work the old node scripts did serially in CI).
- tests/core/helpers/ is excluded from test collection by the include glob
  (`tests/core/**/*.test.ts`); the helper is type-checked via tsconfig's
  `tests` include.
