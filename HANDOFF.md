# T09 HANDOFF - lib/trace: typed boundary around the frozen core

(Replaces the T08 HANDOFF inherited through the branch chain; T08's own
record lives at commit 0acc10a.)

## STATUS

DONE - all DoD checks pass (evidence below). No STOP-gate hit. Mechanism
only: no page wiring, no component/store changes, no frozen-core .js
edits, no page HTML edits, no Rust touched. Segment windowing (T17) and
worker execution (T16) untouched.

## COMPLETED (commits on `ticket/T09-lib-trace-boundary`, on top of 0acc10a)

- `dbc9085` keys.ts + keys.test.ts: parseKey ported from index.html
  (1006-1059) with the `layout: 'known' | 'unknown'` discriminant
  (ADR-0004 section 1 defect fix) + formatEpoch.
- `27af934` title.ts + title.test.ts: traceTitleParams ported from
  index.html (1686-1702).
- `4e4edb4` load.ts + load.test.ts: loadTrace/loadTraceStreamed/
  loadTraceBuffered/parseTraceBuffer/objectTraceUrls + the trace_parser.js
  typed pass-through surface.
- `3587bd7` query.ts + reparse.ts (+ tests): lane-click read helpers and
  the Set/Clear-Range windowed re-parse.
- `27bd5a4` analysis.ts (trace_analysis.js facade) + index.ts barrel;
  src/lib/trace/.gitkeep removed.
- `78292df` scripts/check-core-imports.mjs + package.json wiring
  (check:boundary + pretest).
- (final commit): this HANDOFF.

## WHAT WAS BUILT (and the decisions inside it)

All under `dial9-viewer/ui/src/lib/trace/`, tests colocated. `index.ts`
is the only import surface the rest of `src/` sees (explicit named
re-exports, not `export *`, so a name collision between modules is a
compile error instead of a silently-omitted star export).

### keys.ts (01 I2; ADR-0004 section 1 defect fix)

- `parseKey(key)` -> `{ layout: "known", service, host, bootId, epoch,
  segIndex }` for the #225 layout (date + 5 components), the legacy
  pre-#225 layout (date + 4, bootId ""), and the dateless positional
  fallback (>= 5 components, no date-shaped segment anywhere).
- DEFECT FIX: a key with a date-shaped segment but an undocumented
  component count now returns `{ layout: "unknown", rawKey }` - the
  legacy code positionally shifted columns here (features/01 Finding 1:
  the dev-server's 6-segment demo key showed Service=host-0, Host=abcd).
  Short dateless keys (legacy returned `host=<raw key>`) are also
  `unknown`. The positional fallback survives ONLY where it was
  genuinely best-effort (no date segment at all).
- The legacy result's lazy `traceStart` getter read the page-global
  `useLocalTz`; parsing is now pure - pages call
  `formatEpoch(key.epoch, { localTz })` at render time (ported from
  index.html:988-1004, UTC default like the legacy initial state).

### title.ts (01 I3)

- `traceTitleParams(keys, { localTz? })` -> URLSearchParams with `svc`
  (unique services ", "-joined), `host` (single-host only; multi-host
  drops it), `from`/`to` (min/max epoch; `from` alone for one distinct
  epoch), `segs` (always). Unknown-layout keys contribute only to `segs`
  - the deliberate consequence of the keys.ts defect fix (legacy fed the
  demo key's shifted fields into the title as `svc=host-0`).

### load.ts (02 B12/B14/B17 mechanism, 01 I4)

- `loadTrace(urls, opts)`: STREAM whenever `canStreamDecode()` (single
  URL via fetchTraceStream, multiple via fetchTracesStream), buffered
  fetchTraces+parseTrace fallback otherwise - the B12 selection, minus
  the page chrome. Returns `{ trace, buffer, mode }`; `buffer` is the
  raw gunzipped concatenation (stream path captures chunks while parsing
  and reassembles, exactly the streamAndShowTrace mechanism) so
  Set/Clear-Range re-parses never re-fetch (B14).
- Options are flat (`FetchOptions & ParseOptions`) and split internally;
  headers/signal go to fetch only, parse options to the parser only.
- Page concerns intentionally NOT here: loading labels/timers, loadPerf
  records, AbortError swallowing, the HTTP-401 credentials hint, alerts,
  drop-zone resets. File-drop = `parseTraceBuffer(fileReaderResult)`;
  demo = `loadTrace("demo-trace.bin")`. Credential header INJECTION
  (B17, `Dial9Creds.headers()`) is a caller concern - pass `headers` in.
- `objectTraceUrls(bucket, keys)` ported verbatim from
  index.html:1713-1720 (01 I4).
- Also the typed pass-through of the rest of the trace_parser.js surface
  (EVENT_TYPES, OFF_WORKER_WORKER_ID, formatFrame, symbolizeChain,
  deduplicateSamples, deriveBlockInPlaceGaps + types incl.
  DecodedFieldValue from decode.js). Rule of thumb: load.ts re-exports
  trace_parser.js, analysis.ts re-exports trace_analysis.js.

### reparse.ts (02 E3/E4, B14)

- `reparseWithRange(buffer, { startNs?, endNs? }, opts?)`: in-memory
  re-parse with bounds forwarded only when set (an absent bound is open,
  not 0). `isRangeActive(range)` is the Clear-Range-visibility predicate.
- Tests pin the core's contract for a single open edge: with an active
  filter, `filterEndTime` surfaces as the parser's Infinity default
  (null means "unfiltered") - callers should not assume null.

### query.ts (02 G13/G14)

- `findSpanAt(spans, ns)`: the poll-at-timestamp binary search
  (viewer.html:2618-2626); non-overlapping spans sorted by start.
- `taskAt(polls, ns)`: poll + taskId with the legacy "taskId 0 means no
  task tracking" truthiness preserved (surfaces as null, poll returned).
- `findContainingSpan(allSpans, workerId, ns)` + `spanAncestryAt(span,
  byId, ns)` + `spansById(allSpans)`: the lane-click span focus walk.
  BEHAVIOR NOTE: the legacy cycle guard watched the id SET's size, which
  stops growing once a cycle revisits a span - a parent cycle shorter
  than 1024 hung the page. The port counts steps (same 1024 cap,
  `SPAN_ANCESTRY_CYCLE_LIMIT`); identical results on well-formed chains
  (both stop after 1024 ancestor hops), but real cycles now terminate.
- `enclosingSpans` re-exported typed from the core (it already lives
  there).

### analysis.ts

- Typed re-exports of the trace_analysis.js analysis surface: flamegraph
  builds (buildFlamegraphTree/flattenFlamegraph/buildFgData; heap via
  analyzeAllocations), blocking-call analysis (computeSchedulingDelays,
  computePollWakes), task lifecycle (buildActiveTaskTimeline), worker
  spans (buildWorkerSpans, attachCpuSamples), runtime groups, span data,
  POIs, process CPU series, getTraceTimeRange, hasCpuProfileSamples,
  computeSpanLayout (fence-sitter: may migrate behind lib/canvas when
  the span-panel component lands).
- NOT here (lib/canvas owns them per T08): pixelDownsampleSpans,
  pixelCoverage, makeBarCoalescer, pollHeatmapColor(Quantized),
  flamegraphColor.

### scripts/check-core-imports.mjs (the boundary rule)

- Plain Node (constraint S1, no new deps): fails when any file under
  src/ outside src/lib/trace/ and src/lib/canvas/ imports a ui-root .js
  module (static import, export-from, dynamic import(), require()).
  Exempt: `*.d.ts` (ambient wildcard declarations, no runtime import)
  and `src/types/probe.ts` (T05's tsc-only probe; no Vite input
  references it, never ships).
- Wired as `check:boundary` + `pretest` in package.json: `npm run test`
  runs it first, and the ui CI job already runs `npm run test`
  (.github/workflows/ci.yml `ui` job), so CI enforces the boundary WITH
  NO WORKFLOW EDIT. Negative-tested during development: an import probe
  in src/pages/ and a require probe in src/store/ both exit 1.

## INTEROP

Same pattern T08 documented: plain ESM named imports with relative
specifiers to the core .js files (`import { parseTrace } from
"../../../trace_parser.js"`), typed by the T05 ambient wildcards;
`import type` for types under verbatimModuleSyntax. vite-node resolves
the CJS-guard named exports; nothing in the rollup input graph imports
lib/trace yet, so dist/ is unchanged.

## DoD EVIDENCE

All run in dial9-viewer/ui (npm ci done):

1. `npx tsc --noEmit`: clean (exit 0).
2. `npm run test`: pretest boundary check OK, then 21 files, 251 tests,
   all pass (201 inherited + 50 new: keys 13, title 7, load 11,
   query 12, reparse 4, analysis 3).
3. DoD axes:
   - keys.ts vs documented layouts (keys.test.ts): #225 layout (with and
     without prefix), legacy layout, positional fallback, and the
     unknown discriminant - including the exact features/01 Finding 1
     demo key `traces/2026-04-09/1900/demo-service/local/host-0/abcd/
     1744224000-0.bin.gz` asserted to yield `{ layout: "unknown",
     rawKey }`, NOT shifted fields.
   - title.ts vs features/01 I3 (title.test.ts): single-host case sets
     `host=`, multi-host case drops it; from/to window; segs.
   - load.ts fetch+gunzip+concat (load.test.ts): the test_fetch_traces
     fixture pattern (in-memory gzip of public/demo-trace.bin + stubbed
     global fetch) - raw round-trip, client-side gunzip, mixed
     concat-in-order parsing to 2x events, header forwarding, stream vs
     buffered byte parity, 404 rejection.
   - Boundary check: passes on the tree; fails (exit 1) on injected
     violations (verified for import and require forms).
4. `npm run build`: dist listing unchanged - same 19 files as T07/T08's
   recorded listing (dev-probe chunk + 4 legacy pages + 12 legacy
   scripts + 2 public assets). Local build deletes tracked dist/.gitkeep;
   restored via git checkout before committing (pre-existing quirk, same
   as T06/T07/T08).
5. Not run: cargo build/nextest/clippy (no .rs touched, no trace-format
   change; rust-embed embeds ui/dist, whose listing is unchanged - same
   justification as T06/T07/T08 per the AGENTS.md JS-only rule). No
   test_*.js added, so no scripts/e2e-trace-tests.sh registration needed
   (vitest picks the new suites up via the src/**/*.test.ts include).
   The legacy tests/core/parse_key.test.ts was NOT modified (T15 retires
   it).

## TEST NOTE (for reviewers of load.test.ts)

Byte-parity assertions use Buffer.equals (memcmp) behind a small
`expectBytesEqual` helper: vitest's `toEqual` deep-diffs typed arrays
element-by-element and times out / OOMs the worker on the ~11 MB
gunzipped demo trace. Do not "simplify" back to toEqual.

## EXPECTED MERGE OVERLAPS (sibling worktrees T11/T12)

- `package.json` "scripts": this branch adds `check:boundary` and
  `pretest`. T11/T12 may add their own scripts (e.g. playwright) -
  trivial additive-line merge; keep `pretest` pointing at
  check:boundary.
- Lockfile untouched by this branch (T07's note re T12's playwright
  addition still applies).

## REMAINING

None for T09. Intentionally left for later tickets:

- Page wiring of keys/title/load/reparse/query (index/viewer/flamegraph
  page tickets own their rows; extractPrefix and the listing table stay
  in index.html for its page ticket).
- Segment windowing (`segments.ts`, `aggregates.ts`, architecture 2.8) -
  T17; worker execution of parses - T16; both explicitly out of scope.
- T15: re-point tests/core/parse_key.test.ts consumers at
  lib/trace/keys.ts and retire the extract-from-index.html mechanism.
- The binary-search "first visible index" helper (T08's note): still
  unowned, belongs near the derived-data caches (F5) - not part of
  2.7's file list, so not added here.

## BLOCKERS

None.

## OPEN QUESTIONS (non-blocking, flagged for the integrator)

- Unknown-layout keys now contribute nothing but `segs` to
  traceTitleParams (legacy leaked shifted fields AND their
  filename-derived epoch into `svc`/`from`). If the index page ticket
  wants `from`/`to` for unknown keys, the epoch is filename-derived and
  layout-independent - it could move onto the `unknown` variant later
  (additive, non-breaking).
