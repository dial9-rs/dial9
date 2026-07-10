# T13 HANDOFF - Migrate flamegraph.html (pipeline proof)

(Replaces the FIX-T38 HANDOFF inherited through the branch chain;
FIX-T38's record lives in the history of merge c1d923e.)

Branch `ticket/T13-migrate-flamegraph`, based on the integrated chunk-1
tip c1d923e. Contract: docs/ui-inventory/features/03-flamegraph-html.md
(all sections A-P, ~185 rows incl. the aggregated ?api=1 mode).

## STATUS

COMPLETE. All DoD gates run and green. Scope fence respected: frozen
core untouched, legacy flamegraph.html untouched (its switch line was
already there), no .rs changes, no pushes/PRs.

## COMPLETED (commits on top of c1d923e)

- f260656 feat(viewer): migrate flamegraph.html to a Vite entry (T13)
- a4f541b test(viewer): features/03 row-walker registry + migration ledger (T13)
- e0208dc fix(viewer): seed frozen-core browser globals in bundled entries (T13)
- (final commit: this HANDOFF + verification artifacts note)

## WHAT LANDED

- `dial9-viewer/ui/new/flamegraph.html` - the Vite HTML entry served at
  /new/flamegraph.html (static skeleton + inline styles byte-faithful to
  the legacy page; the only scripts are the module entry and the plain
  /ui-switch.js tag both UI generations share).
- `dial9-viewer/ui/src/pages/flamegraph/` - typed entry:
  - `main.ts` (mode dispatch + switch mount), `dom.ts` (shell elements),
  - `exact-mode.ts` (port of the legacy load path; fetch+gunzip+parse now
    run in the T16 worker pipeline - observable contract unchanged),
  - `api-mode.ts` (faithful port of the ?api=1 poll/refine loop),
  - `query.ts` + `query.test.ts` (pure URL/label logic, unit-tested:
    trace-URL resolution against the origin root, F9 labels, legacy-order
    buildApiUrl/updateBrowserUrl builders).
- Typed seams (boundary check clean): `lib/canvas/flamegraph.ts` (frozen
  widget), `lib/trace/creds.ts` (Dial9Creds), `lib/trace/api_format.ts`
  (flamegraph_api.js helpers + formatHumanDuration - the SAME
  implementation the legacy page runs, so the two generations cannot
  drift), plus d.ts for flamegraph_api.js and the D9UiSwitch global.
- Core-global seed chain (`lib/trace/core-globals.ts`,
  `lib/canvas/core-globals.ts`, `lib/canvas/export-globals.ts`): in a
  bundled ESM page `typeof require` is undefined, so the core's
  factory-time global lookups (TraceParser / TraceAnalysis /
  FlamegraphExport) must be seeded before the wrapped CJS factories run -
  the generalization of the T16 TraceDecoder worker-entry lesson. Every
  direct src importer of trace_analysis.js lists the seed first.
- Registration: one line in ui-switch.js NEW_UI_ENTRIES
  (`"flamegraph.html": "new/flamegraph.html"`), the matching expectation
  in tests/ui_switch.test.ts (acyclicity guard untouched and green), one
  input line in vite.config.ts. The T02/T04 dev-probe placeholder input
  + module were retired (their own headers say page tickets replace
  them; keeping the placeholder made rollup split a shared chunk,
  violating N6's one-bundle-per-page).
- Parity: `parity/walkers/features03.mjs` + registry line in
  walk-rows.mjs. features/03 has NO base live-validation table; its
  2026-07-08 refresh table gates exactly three rows (F171/F176/F177,
  recorded VERIFIED (unit)) - all walked. Everything else maps to
  NOT-TRIGGERABLE per the shared verdict mapping.
- `docs/tickets/ledger.md` (created): +switch-control census entry;
  F4/F5/F6/F166 mechanism-moved-to-worker amendment note.

## DoD EVIDENCE

All against the dev-server on :3051 serving ui/dist (npm run build),
demo seed (bucket demo-traces, prefix traces, 1 folded segment, 147
samples). Machine-readable reports were written under parity/out/
(gitignored); key lines reproduced here.

1. Row-walker, features/03 vs NEW page (/new/flamegraph.html):
   `185 rows - 182 NOT-TRIGGERABLE, 3 VERIFIED. GREEN: zero FAILED`
   - F171 VERIFIED: pickers show 2026-04-09T19:00:07/20:00:08 UTC; Apply
     round-trips both ns params exactly.
   - F176 VERIFIED: stats "147 samples [mdot] 16:36:15 -> 16:36:19 (3.64s)
     [mdot] 1 / 1 files (100.0%) [mdot] 147 samples [mdot] 4.1 MB [mdot] refined".
   - F177 VERIFIED: auto-stopped ("refined" suffix), Stop disabled,
     Refine more enabled, loop stays stopped over 2+ poll intervals.
   - Self-test vs LEGACY page (bare canonical URL): same 3 VERIFIED,
     zero FAILED.
   - NOT-TRIGGERABLE rows: all rows without a gated recorded verdict
     (recorded CODE-READ / VERIFIED (API) / CODE-ONLY / NOT-TRIGGERABLE,
     or no recorded verdict - features/03 was inventoried by code-read,
     not a driven validation pass). Exact-mode behavior is covered by
     gates 2-5 below instead.

2. Affordance census diff (legacy vs new), both modes:
   - exact mode (?trace=demo-trace.bin): 8 vs 8 affordances, ONE diff:
     `CHANGED #d9-ui-switch: label "Switch to new UI" -> "Switch to
     legacy UI", href legacy<->new` - the ledger-justified switch control.
   - api mode (?api=1&bucket=demo-traces&prefix=traces): 15 vs 15
     affordances (toolbar + facet selects included), same single
     #d9-ui-switch diff.

3. Behavioral differ, journey J5 (flamegraph work) legacy vs new:
   `checkpoint rendered: identical (5 fields); checkpoint searched:
   identical (5 fields). ZERO DIFF` (fields: fg.title, fg.stats = sample
   counts, fg.searchStats = tree-derived match stats, fg.canvases,
   url.query).

4. Switch round-trip keeps the trace loaded (playwright, fresh context):
   legacy (147 samples) -> click switch -> /new/flamegraph.html?trace=
   demo-trace.bin (147 samples, stored pref "new") -> click switch ->
   /flamegraph.html?trace=demo-trace.bin&ui=legacy (147 samples, pref
   aligned to "legacy") -> pin-less reload stays legacy. `?ui=new` on
   the canonical URL dispatches to /new/flamegraph.html with the query
   preserved and ui stripped.

5. N10 deep-link + zoom-state parity (legacy vs new, identical on all):
   - multi-trace `?trace=a&trace=b` + svc/host/segs/from/to: title and
     header "Flamegraph [mdash] demo-service @ local", stats "294
     samples [mdot] 2 segments [mdot] 19:00 -> 19:10" (multi-URL
     pipelined streaming inside the worker).
   - click-zoom writes `worker-zoom` (same frame path both sides),
     reload restores the breadcrumb, Escape resets zoom and cleans the
     URL back to `?trace=demo-trace.bin`.

6. Existing flamegraph export tests green: tests/core/
   flamegraph_export.test.ts (+ flamegraph_api / flamegraph_recipes)
   run inside the full `npm run test` pass below.

7. Bundle within N6: legacy page total JS payload 299,006 bytes
   (inline bootstrap 32,884 + 9 script files incl. decode/trace_parser/
   trace_analysis/flamegraph*/creds/format/ui-switch). New page:
   ONE self-contained page bundle 91,868 bytes (min; 32.05 kB gzip)
   + worker chunk 30,097 + shared unbundled ui-switch.js 18,642
   = 140,607 bytes total, 47.0% of the legacy payload (page bundle
   alone 30.7%). No shared/split chunks.

8. Formal gates: `npx tsc --noEmit` clean; FULL `npm run test` green
   (see final commit message for the run tallies - includes the new
   query.test.ts and the updated ui_switch.test.ts registry +
   acyclicity guard, unweakened); `npm run build` clean;
   `npm run check:boundary` OK; `cargo build -p dial9-viewer` OK.
   JS/HTML-only change: no .rs touched, no trace-format change, so no
   cargo nextest / stress run (per AGENTS.md); cargo fmt/clippy N/A.

## DECISIONS (and why)

- Section P (api mode) implemented here per the recorded default (T13
  owns features/03 ALL rows); the maintainer-confirmation flag from
  T01's HANDOFF stands, but the page cannot ship without api-mode
  parity anyway. If ownership moves next to the aggregation tickets
  later, the api-mode code is already isolated in api-mode.ts.
- Exact mode loads through loadTraceInWorker (T16) rather than
  main-thread loadTrace: highest-fidelity reuse of the audited
  pipeline, kills the parse stall, and keeps decode.js out of the main
  bundle. The observable contract (labels, errors incl. the 401 hint,
  analysis, rendering) is unchanged - ledger line records the mechanism
  amendment.
- Trace URLs resolve against the ORIGIN ROOT (query.ts
  resolveTraceUrls): the canonical page lives at /, so its relative
  ?trace= values are root-relative; without this, /new/-page-relative
  or worker-script-relative resolution would silently break deep links
  (unit-tested).
- api-mode fetch loop ported raw (not the lib/trace aggregates client):
  the legacy page surfaces a 404 as an error; the client maps it to
  "unavailable". Parity wins; the client remains for chunk-2 consumers.
- flamegraph_api.js helpers are REUSED (typed re-export via lib/trace),
  not re-ported: one implementation on both sides of the switch while
  both pages are servable. isCoverageFrozen deliberately NOT
  re-exported (aggregates.ts's typed port is the src/-side
  implementation; a barrel collision would otherwise be silent).
- dev-probe input + module retired (see WHAT LANDED). If T14 lands
  after this, their input-line addition merges trivially.
- F171 walker uses nonzero seconds: browsers normalize a datetime-local
  value by dropping trailing :00 seconds on read-back, which is not the
  row's contract.

## REMAINING / BLOCKERS

None for the DoD. Notes for the integrator:

- MERGE (shared files, expected trivial): ui-switch.js NEW_UI_ENTRIES
  (one line), tests/ui_switch.test.ts registry expectation (T14 extends
  the same array), vite.config.ts input map (T14 adds a line; the
  dev-probe line was removed here).
- The proxy dev loop (`npm run dev` + backend on :3001) was not smoke
  tested; all gates ran against the built dist on the dev-server (the
  documented parity path).
- features/03 anchor refresh (Source column pointing at the new page
  files) is inventory upkeep left to the legacy-removal ticket (T39),
  per the ledger amendment note.

## OPEN QUESTIONS

- Section P ownership confirmation (T01 HANDOFF flag) - proceeded on
  the recorded default (T13 owns it); no code change needed either way.
- The api-mode header/tab title in BOTH generations reads "Flamegraph
  [mdash] aggregated" when no service param is present (the server
  sends no host_names; the legacy branch reading it is preserved but
  dead against the current server) - contract-faithful, flagged for the
  chunk-2 UX pass.
