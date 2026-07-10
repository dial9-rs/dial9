# T16 HANDOFF - Web Worker load pipeline

(Replaces the T09 HANDOFF inherited through the branch chain; T09's own
record lives at commit 9fd0cb3.)

## STATUS

DONE - all DoD checks pass (evidence below). No STOP-gate hit. Scope
fence respected: no UI, no segment windowing (T17), no frozen-core .js
edits, no Rust code touched. The whole-trace worker path is built; T17
window-izes it.

## COMPLETED (commits on `ticket/T16-worker-load-pipeline`, on top of 9fd0cb3)

- `70c87e0` stream.ts: the fetch-stream + chunk-capture + reassembly
  mechanism extracted out of loadTraceStreamed into a leaf module (public
  load.ts surface unchanged) so the worker body can share it without
  importing load.ts - load.ts also hosts the orchestrator's
  `new Worker(new URL(...))` reference, and importing it from the body
  would put the worker entry inside its own bundle graph.
- `52fae8c` worker/protocol.ts (types-only message contract),
  worker/body.ts (pure pipeline: fetch INSIDE the worker, gunzip+parse,
  capture), worker/trace-worker.ts (browser binding),
  worker/node-worker-entry.mjs (worker_threads binding),
  tsconfig `allowImportingTsExtensions`, worker/body.test.ts.
- `5e4b355` load.ts loadTraceInWorker orchestrator + barrel exports +
  load.worker.test.ts (fake-port contract tests incl. the three abort
  observables against a real T07 store).
- `be00eb2` worker/integration.test.ts (real worker_threads thread + real
  http fetch + real structured-clone hop + real store);
  src/lib/trace/package.json ({type:module} scope, see below).
- `ad14f68` dev-probe build wiring + vite.config commonjs interop fixes
  (the bundled-core landmines, see below) + TraceDecoder seed in
  trace-worker.ts.
- (final commit): this HANDOFF.

## CLONEABILITY VERDICT (first work item)

ParsedTrace IS structured-cloneable as-is; NO snapshot shape needed.
Verified by parsing public/demo-trace.bin with the frozen core under Node
and walking the full output (functions / accessor properties / class
instances / symbol keys / typed arrays: ZERO of each), then
structuredClone() of the whole 294,465-event parse: succeeds (~550 ms),
Maps round-trip AS Maps (11 Maps: spawnLocations, taskSpawnLocs,
taskSpawnTimes, taskTerminateTimes, taskInstrumented, callframeSymbols,
threadNames, tidToWorker, runtimeWorkers, segmentMetadata, taskDumps),
counts + spot samples equal. Custom-event field values may be bigint
(decode.js DecodedFieldValue union) - bigint is also clone-safe. The
features/01 parseKey getters are page code, not core output (confirmed).
Documented in worker/protocol.ts's header; types/trace.d.ts needed no
change (no snapshot type exists because none is needed).

## WHAT WAS BUILT

### The worker path (architecture 2.7/2.8; ADR-0004 section 6 "do now")

- `lib/trace/worker/protocol.ts` - types-only wire contract:
  load/abort requests; progress/done/error responses. Progress carries
  the features/02 B load-timing fields: phase (fetching|parsing), mode
  (stream|buffered), urlCount (B8 multi-trace labels), bytesRead,
  totalBytes (null while streaming), eventCount (B8), startMs + elapsedMs
  (B9). Done carries the B16 worker-measurable timing record
  {startMs, fetchDoneMs (buffered only; null in stream mode - legacy
  loadPerf parity), parseDoneMs, mode, events, bytes}. `totalMs` is
  deliberately page-side (defined as start->render-complete via
  double-rAF). CLOCK NOTE: worker performance.now() has its own
  timeOrigin - consumers use deltas, never mix with main-thread marks.
- `lib/trace/worker/body.ts` - the pipeline as a PURE module
  (createWorkerBody(post)): mode selection via canStreamDecode inside the
  worker, stream path via the shared streamTraceWithCapture, buffered
  path via fetchTraces+parseTrace with a fetch-done mark, progress
  forwarding (the core's per-100KB onParseProgress), one body = one load.
  The body's AbortController aborts the in-flight fetch on an "abort"
  request (cooperative); hard kill is the orchestrator's terminate.
- `lib/trace/worker/trace-worker.ts` - browser binding (Vite worker
  entry) + the TraceDecoder global seed (see landmines).
- `lib/trace/worker/node-worker-entry.mjs` - worker_threads binding for
  Node tests, loading body.ts via Node's NATIVE TYPE STRIPPING
  (>= 22.18; CI runs Node 24, local v25). This is the chosen Node-shim
  path per the ticket (browser mode would drag Playwright deps - T12's
  turf).
- `load.ts` gains `loadTraceInWorker(store, urls, opts)`: spawns one
  worker per load via the Vite-detected inline
  `new Worker(new URL("./worker/trace-worker.ts", import.meta.url),
  { type: "module" })` (transport injectable via opts.worker for tests),
  forwards progress, updates the store's `trace` slice on done, then
  resolves; terminates the worker on ANY settle (no live handle after
  success, error, or abort). Barrel exports: loadTraceInWorker +
  TraceSliceStore/WorkerLoadOptions/WorkerLoadResult/WorkerTraceLoad +
  the protocol's port/progress/timing types.

### Abort semantics (decided here, as the ticket required)

ONE AbortController per load, owned by the orchestrator. handle.abort()
and the optional external opts.signal (the page's Escape/Back) both
funnel into it. On abort: (1) post {kind:"abort"} into the worker -
cooperative fetch cancellation; (2) port.terminate() - authoritative,
also kills a compute-bound parse that no signal reaches; (3) reject
`done` with DOMException("AbortError"). After settle, late worker
messages are DROPPED: no progress callback fires, the store is never
touched. `done` has a no-op rejection handler pre-attached so
fire-and-forget pages never produce an unhandled rejection (awaiting
callers still get the rejection; AbortError swallowing stays a page
concern, B10).

### Store coupling

`TraceSliceStore` = { update("trace", { trace }) } - a minimal structural
interface in load.ts, so lib/trace does not import src/store.
Compile-time-checked that ViewerStore satisfies it
(load.worker.test.ts). Tests drive real createStore instances with an
injected microtask scheduler (Node has no rAF).

### Plain-Node-runnable worker chain (the enabling constraint)

node-worker-entry.mjs -> body.ts -> stream.ts + trace_parser.js runs
under plain Node with NO bundler: every runtime import on that chain
resolves on disk as written. Consequences, all documented in file
headers:
- body.ts imports `../stream.ts` with an explicit .ts extension ->
  tsconfig gains `allowImportingTsExtensions: true` (legal because noEmit;
  Vite/vitest resolve .ts specifiers natively). Everything else keeps the
  .js-specifier convention; type-only imports are erased by the stripper
  and exempt.
- `src/lib/trace/package.json` ({type:module} + explanatory "//"): scopes
  ESM module type for plain Node so the type-stripped chain loads without
  MODULE_TYPELESS_PACKAGE_JSON warnings. The ui ROOT package.json stays
  typeless ON PURPOSE (constraint H2: `node test_*.js` root scripts are
  CJS). Vite/vitest/tsc do not consult the nested file.

### Bundled-core landmines found and fixed (vite.config.ts)

Nothing had ever pulled the frozen core through a ROLLUP build before
(T09: "nothing in the rollup input graph imports lib/trace"); the worker
entry does. Two real breakages surfaced, both fixed WITHOUT touching the
core:

1. BUILD FAILURE: rollup's CJS interop (commonjsOptions.include) covers
   node_modules only by default, so named ESM imports from the CJS-guard
   core failed ("fetchTraceStream is not exported by trace_parser.js").
   Fix: include `"*.js"` (ui root, cwd-relative) AND
   `"../../dial9-trace-format/js/*.js"` - decode.js at the ui root is a
   SYMLINK and Vite ids modules by realpath.
2. RUNTIME FAILURE (bundle-only): trace_parser.js getTraceDecoder does
   `require(path.resolve(__dirname, "decode.js"))` under Node, falling
   back to the TraceDecoder browser global. The default commonjs
   transform rewrites `typeof require` to a DEFINED throwing helper, so
   the bundled worker always took the require branch and threw at first
   parse. Fix: `ignoreDynamicRequires: true` keeps `require` a
   genuinely-undefined bare identifier in module scope, plus
   trace-worker.ts seeds `globalThis.TraceDecoder` from the bundled
   decode.js before any message arrives - the same resolution order the
   legacy <script src> pages use. Node/vitest paths run the real CJS
   files and are unaffected.

VERIFIED by running the EMITTED dist/assets/trace-worker-*.js chunk under
a worker_threads Web-Worker shim (scratch harness, not committed): full
parse parity on the demo trace (294,465 events, Maps intact, buffer
byte-exact, 45 progress messages, stream mode). Harness note: the chunk
must be loaded as ESM (.mjs copy) - loading the .js under the typeless ui
package.json makes Node CJS-load it and define `require`, which is a
harness artifact, not bundle behavior.

### Test layer map (for T12/T13 verification)

- worker/body.test.ts (6): pipeline logic IN-PROCESS - stubbed fetch,
  no thread. Stream parity vs direct parse, buffered fallback phases +
  fetch mark (DecompressionStream stubbed out; the core's zlib fallback
  still gunzips), wire parse options, abort-request fetch cancellation,
  one-load-per-worker, 404 propagation.
- load.worker.test.ts (8): orchestrator contract vs a scripted fake port
  + real store. Message shapes, done->store->resolve->terminate ordering,
  the three abort observables, external + pre-aborted signals, error
  name preservation, transport error, unhandled-rejection hygiene.
- worker/integration.test.ts (3): the REAL boundary - worker_threads
  thread (execArgv:[] so vitest flags never leak), fetch INSIDE the
  worker against a local http server, actual postMessage structured
  clone, real store. Parity + progress, abort mid-parse, cross-thread
  error propagation.
- BROWSER-ONLY REMAINDER: the Vite `new Worker(new URL(...))` entry
  detection/URL rewriting in a live page. The emitted chunk itself was
  smoke-verified (above); the live-page path lands with T13's page entry
  (T12's parity harness can drive it).

## DoD EVIDENCE

All run in dial9-viewer/ui (npm ci done), final tree:

1. `npx tsc --noEmit`: clean (exit 0).
2. `npm run test`: pretest boundary check OK (worker/ subdir is inside
   the allowed src/lib/trace/ prefix - no ALLOWLIST change needed), then
   24 files / 268 tests pass (251 inherited + 17 new: body 6,
   orchestrator 8, integration 3).
3. DoD check "worker parity": worker/integration.test.ts parses the demo
   trace through the real worker path and compares against a direct
   parseTrace - counts (events, cpuSamples, customEvents, allocEvents,
   blockInPlaceGaps, 4 Map sizes) + spot samples (first/mid/last event,
   mid custom event, a callframeSymbols entry, minTs/maxTs,
   hasTaskTracking) + byte-exact transferred buffer. Deliberately NOT
   whole-object toEqual (T09's vitest trap on huge arrays).
4. DoD check "progress": same test asserts >1 progress events, every
   load-timing field populated on each (phase/mode/urlCount/bytesRead/
   totalBytes-contract/eventCount/startMs/elapsedMs), live counters > 0
   by the last message, and the done timing record's field relations.
5. DoD check "abort mid-parse": abort issued only after parsing is
   demonstrably mid-flight (bytesRead > 0). The three named observables:
   (a) worker terminated - the thread's exit event is awaited (a live
   handle would time the test out); (b) store trace slice unchanged -
   getState().trace.trace stays null, re-checked after a 100 ms drain;
   (c) no pending progress callbacks after abort - counted zero. Also
   pinned transport-independently in load.worker.test.ts.
6. `npm run build`: dist delta vs the T09-recorded 19-file listing is
   EXACTLY +1 file: dist/assets/trace-worker-<hash>.js (29.08 kB, the
   worker chunk with the core bundled inside - expected, it is built
   code) and the dev-probe chunk grows (0.00 kB -> 37.80 kB) because the
   probe now pulls the barrel through the build (see decisions). No stray
   assets: node-worker-entry.mjs and the nested package.json do NOT enter
   dist. Local build deletes tracked dist/.gitkeep; restored via git
   checkout before committing (same pre-existing quirk as T06-T09).
7. `cargo build -p dial9-viewer`: exit 0 (run because the dist listing
   changed; rust-embed re-embeds ui/dist). No cargo nextest / stress /
   clippy / fmt: no .rs touched, no trace-format change (AGENTS.md
   JS-only rule). No new ui-root test_*.js, so no e2e-trace-tests.sh
   registration (vitest auto-discovers src/**/*.test.ts).

## DECISIONS A REVIEWER SHOULD SEE

- dev-probe.ts re-exports loadTraceInWorker: the ONLY way `npm run
  build` exercises the Vite worker bundling before a real page migrates
  (the ticket's build check anticipated the worker chunk appearing).
  Page tickets replace the probe with real entries.
- One worker per whole-trace load, terminated on settle. T17's
  per-segment tier will want a persistent worker (or pool) - the
  TraceWorkerFactory seam and the pure body are built for that; only the
  one-load guard and orchestrator lifetime need revisiting.
- The body posts AbortError as a normal error message (no special case);
  the orchestrator's settled-guard is what guarantees silence after
  abort. Keeps the body dumb and the invariant in one place.
- Progress messages are NOT throttled beyond the core's per-100KB cadence
  (~110 msgs / 11 MB; tiny payloads). If a page ever needs coarser
  cadence, throttle at the onProgress consumer.
- loadPerf `mode:"local"` (file drop) and `"reparse"` stay OUTSIDE the
  worker path for now: file-drop hands a main-thread buffer to
  parseTraceBuffer, reparse is in-memory (reparse.ts). Moving buffer
  parses into the worker is trivial protocol growth (a parse-buffer
  request kind with a transferred buffer) if a page ticket wants it.

## EXPECTED MERGE OVERLAPS (sibling worktrees T12/T18)

- `src/lib/trace/index.ts`: T18 adds its own barrel lines (aggregates
  client). Trivial additive merge; keep the explicit named-export style.
- `package.json` scripts: untouched by this branch (T09's pretest note
  still applies for T11/T12).
- vite.config.ts: this branch adds build.commonjsOptions; T12 may touch
  test config. Additive-block merge.
- tsconfig.json: this branch adds allowImportingTsExtensions (+comment).

## REMAINING

None for T16. Flagged for later tickets:

- T17: segment windowing on top of this path (persistent worker/pool via
  the TraceWorkerFactory seam; stale-fetch discard already has its
  primitive in the abort protocol).
- T13/T14 (page entries): when a bundled PAGE parses on the main thread
  through lib/trace, it needs the same TraceDecoder global seed the
  worker entry has (or a shared side-effect module) - the legacy pages
  get it from <script src> ordering, bundles do not. Also inherit the
  worker chunk into their build output and the live-page worker
  verification (T12 harness).
- The barrel-through-page bundle currently duplicates the core into both
  the page chunk and the worker chunk (~30 kB min each). Fine for the
  probe; page tickets may want a manualChunks split if it matters.

## BLOCKERS

None.
