# T18 HANDOFF - lib/trace/aggregates: tier-1 aggregates client

(Replaces the T09 HANDOFF inherited through the branch chain; T09's own
record lives at commit 9fd0cb3.)

## STATUS

DONE - all DoD checks pass (evidence below). No STOP-gate hit. Client
mechanism only: no consuming UI (minimap etc. = chunk 2), no server
changes, no .rs edits (the verified dev-server recipe made the
"extend dev_server.rs" exploration unnecessary), no frozen-core .js
edits. All dev-servers started for fixture capture were killed (port
3041 verified free after each run).

## COMPLETED (commits on `ticket/T18-aggregates-client`, on top of 9fd0cb3)

- `4b1c4ac` fixtures: 6 recorded JSON responses + 1 real 404 text body
  under `dial9-viewer/ui/tests/fixtures/aggregates/`.
- `2bc905b` `src/lib/trace/aggregates.ts` + barrel exports in
  `src/lib/trace/index.ts`: wire types for both endpoints (derived from
  src/server/flamegraph.rs, tokio_stats.rs, src/ingest/aggregate.rs),
  URL builders (repeatable `host=`, `refine=true` literal - serde bool,
  never `1`; string-typed ns bounds for >2^53 precision), injectable
  fetch, the AggregateResult union with the CoverageSignal
  (full/partial/none), AggregatesRequestError for non-404 failures, and
  `refineUntilFrozen`.
- `46e6fe5` `src/lib/trace/aggregates.test.ts`: 28 vitest tests, fixture
  recipe documented in the header.
- (final commit): this HANDOFF.

## DESIGN DECISIONS (within pre-ruled bounds)

- Coverage signal rule (documented on `coverageSignal`):
  no coverage block -> `full` (non-demand-driven single fetch, the
  legacy page's interpretation at flamegraph.html:415-420);
  `files_matched === 0 || files_folded === 0` -> `none`;
  folded < matched on files OR hosts -> `partial`; else `full`.
- Refine-loop termination (documented on `refineUntilFrozen`): one
  read-only poll, then refine polls; stop when FROZEN (`files_folded`
  did not increase between consecutive polls - mirrors
  flamegraph_api.js:56-61 isCoverageFrozen semantics, reimplemented; the
  page-adjacent file is NOT imported into src/) or at the CEILING
  (default `DEFAULT_MAX_REFINE_POLLS = 30` refine polls, exported).
  The legacy plateau heuristic (shouldAutoStopRefining) is deliberately
  NOT implemented - noted in the doc comment as a chunk-2 consumer
  policy (layerable via `onResult` + abort).
- No built-in pacing (legacy page waits 800ms between polls - a UI
  policy); consumers pace inside the `poll` closure. Documented.
- The server tree type is exported as `ApiFlamegraphNode`: the barrel
  already exports the frozen core's differently-shaped `FlamegraphNode`
  (trace_analysis.js), and the barrel's explicit-named-exports rule
  makes that collision a compile error otherwise.
- 401/403/421/500 throw `AggregatesRequestError` (status + body); only
  404 and flag-false are "no data" per the ticket's pre-ruling.

## FIXTURE RECIPE (reproducible; captured 2026-07-10; also in the test header)

The stock dev-server's aggregate endpoints are functional against its
seeded demo bucket - no dev_server.rs changes:

    cd dial9-viewer/ui && npm run build
    CARGO_TARGET_DIR=<repo>/target PORT=3041 \
      cargo run -p dial9-viewer --bin dev-server --features dev-server

Capture log (fold state is per server run; restart between endpoint
sequences so both colds are genuinely cold):

    curl 'http://localhost:3041/api/flamegraph?bucket=demo-traces&prefix=traces'
      -> flamegraph-cold.json    638 B   (folded 0/1, empty tree, total_samples 0)
    curl '...&refine=true'
      -> flamegraph-refine.json  124906 B (folded 1/1, hosts 1/1, total_samples 147,
                                           facets populated: source=[cpu,sched])
    curl (read-only again)
      -> flamegraph-warm.json    124906 B (identical folded counts -> frozen)
    (restart server)
    curl 'http://localhost:3041/api/tokio-stats?bucket=demo-traces&prefix=traces'
      -> tokio-stats-cold.json   199 B   (folded 0/1, total_polls 0)
    curl '...&refine=true'
      -> tokio-stats-refine.json 32490 B (folded 1/1, total_polls 94212)
    curl (read-only again)
      -> tokio-stats-warm.json   32490 B (frozen)
    curl '...prefix=no-such-prefix' (flamegraph)
      -> not-found-no-match.txt  32 B    (REAL 404, text/plain,
                                          "no source files match this scope";
                                          tokio-stats returns the identical
                                          status/body, verified on the wire)

The no-agg-context 404 flavor is NOT producible from this dev-server
(it always allows BYO creds, so `agg_context_for` succeeds for any
bucket param); tests synthesize it with a stubbed fetch, shape-checked
against the handlers' `(StatusCode::NOT_FOUND, String)` rejections
(tokio_stats.rs:80-84, flamegraph.rs:203-208): plain-text body, 404.

## DoD EVIDENCE

- `npx tsc --noEmit`: clean (exit 0).
- `npm run test`: 22 files / 279 tests passed (includes pretest
  boundary check: "check-core-imports: OK"). The new suite alone:
  28/28 passed.
- Refine-sequence check: `refineUntilFrozen` driven over the recorded
  cold/refine/warm fixtures terminates frozen in exactly 3 requests
  for BOTH endpoints, with refine params `[absent, true, true]` on the
  wire and progressive signals none -> full -> full via onResult.
- Degradation matrix (each without throwing):
  - 404 no-agg-context (synthetic) -> unavailable(not-found), coverage
    none, body text surfaced as `message` - both endpoints;
  - 404 no-files-match (real recorded body) -> same shape;
  - aggregation_enabled=false -> unavailable(disabled), coverage none,
    fetch stub provably never invoked;
  - partial counts (12/480 files, 8/40 hosts) -> data with coverage
    "partial"; plus the full unit matrix for coverageSignal /
    isCoverageFrozen (none/partial/full, frozen/not-frozen edges);
  - 500 still throws AggregatesRequestError (status + body preserved).
- `npm run build`: dist file listing byte-identical before/after
  (19 files; aggregates.ts is not a Vite input - `find dist -type f`
  diff empty). rust-embed unaffected (dist unchanged); dial9-viewer
  compiled fine during the dev-server runs.
- Boundary: aggregates.ts lives in `src/lib/trace/` (allowed dir);
  check-core-imports passes; flamegraph_api.js not imported.

## REMAINING

None for T18.

## BLOCKERS / NOTES FOR INTEGRATION

- Barrel merge overlap (expected, trivial): T16 also appends a section
  to `src/lib/trace/index.ts` in another worktree. T18's block is
  appended at the end of the file (after the analysis.ts section).
  Resolve by keeping both sections.
- `Coverage`/`CoverageSignal` types live in aggregates.ts and are
  barrel-exported; chunk-2 consumers (minimap, tokio-stats page) should
  consume the signal, not recompute from counts.
- tokio-stats quirk carried into the types: its `coverage.samples_folded`
  is files READ this request (server comment, tokio_stats.rs:184),
  documented on `TokioStatsResponse.coverage`.
