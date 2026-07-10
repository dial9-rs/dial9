# T17 HANDOFF - Segment-windowed loading (tier 2 core)

(Replaces the T16 HANDOFF inherited through the branch chain; T16's own
record lives at commit 357ac5e.)

## STATUS

DONE - all DoD checks pass (evidence below). No STOP-gate hit. Scope
fence respected: no UI/minimap, no aggregates client (T18 lives on a
sibling branch and was NOT re-implemented), no frozen-core .js edits, no
page HTML, no .rs touched.

## COMPLETED (commits on `ticket/T17-segment-windowing`, on top of 357ac5e)

- `a9dcf7e` worker parse-buffer request: protocol.ts grows
  TraceWorkerParseBufferRequest (buffer CLONED in - the sender's copy is a
  live cache entry; decompressed buffer transferred back); body.ts
  refactor hoists shared makeReporter/startJob out of runLoad (comments
  carried) + runParseBuffer (DecompressionStream gunzip-stream fused with
  parse; explicit error where gzipped bytes meet a no-DS runtime - the
  core's zlib fallback would throw a bare-require ReferenceError in a
  browser worker); stream.ts extracts parseChunksWithCapture (public
  surface unchanged); 4 new body tests.
- `a39cd10` additive types: types/trace.d.ts boundary-poll vocabulary
  (SegmentEdgePolls, WindowEdgePoll with explicit `taskId: null` for
  left-edge crossings, StitchedBoundaryPoll, WindowBoundaryPolls);
  types/state.d.ts SegmentEntry gains trace / rawByteLength /
  invariants (SegmentParseInvariants, retained across eviction) /
  edgePolls. T06 union + existing fields untouched.
- `ab45f27` lib/trace/segments.ts (the 2.8 placement) + barrel exports +
  load.ts exports defaultTraceWorkerFactory for the per-segment driver.
- `fcd9507` segments.test.ts: 42 (now 43) pure-function cases + a
  real-demo-parse anchor.
- `1e3a29c` reservation-aware eviction (design fix found while writing
  the DoD eviction test): planEviction gains reservedBytes so reconcile
  frees room BEFORE an admitted parse lands; without it resident could
  transiently overshoot the hard budget by one segment.
- `f09e993` segments.window.test.ts (17 orchestrator/hard-edge/scenario
  tests) + one real worker_threads parse-buffer parity case in
  worker/integration.test.ts.
- (final commit): this HANDOFF.

## WHAT WAS BUILT

### Budgets (N19; shared-decisions constants, all tunable via options)

RESIDENT_RAW_BUDGET_BYTES = 128 MB (sum of DECOMPRESSED sizes of parsed
segments - the proxy for the ~10x parsed heap), RAW_GZIP_CACHE_BUDGET_BYTES
= 256 MB, BUDGET_EVICTION_THRESHOLD_FRACTION = 0.9 (the 10% headroom is
the hysteresis margin; GC lags eviction). GZIP_EXPANSION_ESTIMATE = 4 is
the pre-fetch planning estimate ONLY (real sizes replace it after the
first parse; chosen above the observed ~3.3x so reservations over-cover).

### The decision layer (pure, exhaustively tested)

- deriveSegmentExtents: heatmap parity (features/01 F2/F4/F7 + I2):
  filename-epoch start, last_modified end, 1s floor, tiled ends;
  unrecognized DIRECTORY layouts still derive via the basename pattern
  (the dev-server's 6-component key works); keys with no epoch are
  returned in `skipped` with a reason, never guessed. Extents are
  wall-clock ns; mapExtentToMonotonic(extent, clockOffsetNs) moves them
  into the viewport's domain once a parse provides the offset. CLOCK
  DOMAIN NOTE: everything handed to the orchestrator must share one
  domain (module-header doc).
- computeNeedSet (closed-interval overlap - touching edges count),
  computePrefetchSet (+/-1 beyond both edges; nearest neighbors on both
  sides when the viewport sits in a coverage gap).
- capToBudget: admission against the trigger (the N19 window limit, not
  a rejection - the nearest need segment is always admitted, and the
  legacy 100/200 MB open cap has no successor check anywhere else).
- planEviction: farthest-from-viewport first ("LRU by distance", 2.8)
  past the 90% trigger; need/prefetch protected there; prefetch, then
  farthest-from-center need members evicted ONLY to re-clamp under the
  HARD budget; reservedBytes (admitted-but-unparsed estimates) shrink
  both watermarks so eviction precedes the parse that needs the room.
- createRawByteCache: true-LRU (access-ordered) gzip byte cache, own
  budget, same 90% trigger; entries larger than the trigger are refused
  (caching one would empty the cache and still not fit).

### Boundary polls (the 2.8 truncation hard edge; features/02 G5)

Per-segment: computeSegmentEdgePolls extracts, in one scan, polls left
open at the segment END (exactly what the core discards at trace end,
trace_analysis.js #194) and dangling PollEnd-first closes at the START
(what the core ignores). Park/Unpark-first starts are ambiguous and
deliberately captured as nothing (ADR-0002 spirit: absence is never
fabricated). Stored on the entry (edgePolls), dropped with the parse on
eviction; segmentInvariants (minTs/maxTs/workerIds) persist eviction.

Window-level: computeWindowBoundaryPolls walks maximal runs of
consecutively-listed parsed segments; internal boundaries STITCH matched
open/close pairs into complete polls; run edges with a listed-but-
unparsed neighbor surface as WindowEdgePoll {truncatedAt, openEnded:
true}, spans clamped to observed events (a lower bound - never a
long-poll false positive, never rendered to the window edge). Absolute
listing ends keep core trace-edge parity (dropped, no marker). Left-edge
crossings carry `taskId: null` - the PollStart holding task identity was
never parsed; renderers must treat it as explicitly unknown.

ADR-0002 note: block-in-place gap detection runs inside each segment's
parse over that segment's events only, so window edges ARE trace edges
for gap detection; nothing to do, documented in the module header.

### Worker path

parseSegmentInWorker drives the new parse-buffer request: one worker per
segment parse, terminated on settle, late messages dropped - T16's
orchestration semantics reused wholesale (the persistent pool the T16
HANDOFF anticipated was NOT needed; worker spawn is negligible next to a
segment parse, and the TraceWorkerFactory seam keeps the pool option
open). Bytes are CLONED into the worker (the cache entry survives); the
decompressed buffer comes back transferred and only its byteLength is
kept (the budget cost); gzipped input streams through DecompressionStream
so gunzip and parse overlap.

### The orchestrator (createSegmentWindow)

Takes the store (SegmentsSliceStore - structural, compile-checked against
ViewerStore), the ordered ListedSegment[] and options (fetch/parser/idle
all injectable; urlFor defaults to identity - the key IS the URL for
non-S3 `trace=` sources; S3 callers pass an /api/object mapping). Seeds
every segment as "listed"; setViewport drives
listed -> fetching -> parsed -> evicted (-> fetching ...), where
"fetching" covers the whole in-flight job and an abort reverts to the
pre-flight state ("evicted" iff previously parsed). Reconcile pass:
need/prefetch -> budget admission -> stale-job aborts -> reservation-
aware eviction -> start need jobs now, prefetch via the idle scheduler
(callbacks re-verify the viewport before starting). Parse completions
write the entry (trace + rawByteLength + invariants + edgePolls) and
re-reconcile. Failures revert the entry, surface via onError (default
console.warn), and are NOT hot-retried - the mark clears when the key
leaves the wanted set. Keys evicted by the hard clamp are not restarted
in the same pass (their real sizes make the next admission defer them
honestly - no fetch/evict loop).

## DoD EVIDENCE

All run in dial9-viewer/ui (npm ci done), final tree:

1. `npx tsc --noEmit`: clean (exit 0).
2. `npm run test`: pretest boundary check OK, then 26 files / 333 tests
   pass (268 inherited + 65 new: 43 pure, 17 orchestrator, 4 body
   parse-buffer, 1 worker_threads integration).
3. HARD EDGE "abort on viewport jump": segments.window.test.ts pins both
   phases - an in-flight FETCH is aborted (its AbortSignal observed
   fired), the entry reverts to "listed", and a late transport
   resolution reaches neither the parser nor the store; an in-flight
   PARSE job receives abort() and its late completion never mutates the
   store (resident stays 0). Transport-independent driver semantics
   (abort message + terminate + AbortError + late-message drop) pinned
   separately against a scripted port.
4. HARD EDGE "boundary poll truncated, not long": with segments 0+1
   parsed and 2 unfetched, the poll opening at the end of segment 1 is
   surfaced as {truncatedAt: "end", openEnded: true} with its span
   clamped to the segment's last observed event - and the same fixture
   widened over segment 2 resolves it into a stitched complete poll.
   Pure-layer tests additionally pin left-edge (taskId null) crossings,
   absolute-listing-edge core parity, evicted-neighbor edges, unmatched-
   evidence drops, and a real-demo-parse consistency anchor.
5. HARD EDGE "re-entry hits raw cache not network": after eviction, re-
   entering the window re-parses with the mock fetch count for that
   segment still 1 (cacheHits 1, parser invoked twice); shrinking the
   gzip cache budget until LRU eviction drops the bytes makes re-entry
   fetch exactly once more.
6. HARD EDGE "eviction at the 90% threshold, resident under budget":
   pure tests pin the exact boundary (900/1000 does not trigger, 901
   does; reservation of 300 shrinks the trigger to 600) and the order
   (farthest first, protection tiers, hard re-clamp, deterministic
   ties); the orchestrator test steps a 6-segment walk asserting
   resident and peak <= budget at every step, evictions fired, and
   evicted entries retaining invariants + learned sizes.
7. DoD 10-SEGMENT SCENARIO (repeated-demo-style fixtures at recorded
   sizes; real encoder fixtures arrive with T42): 10 segments x 30 MB
   recorded raw (10 MB gzip listing), viewport walked 0 -> 9 with idle
   prefetch, then back 9 -> 0. Measured: resident raw peaked at
   94,371,840 B (90 MB) <= 128 MB budget, asserted <= budget after every
   step; forward pass: 10 network fetches, 10 parses, 7 evictions;
   after the back pass: networkFetches STILL 10 (zero re-downloads),
   17 parses total (7 cache-hit re-parses), 14 evictions, peak unchanged
   at 90 MB. Scaling argument: the budget accountant only ever does
   byteLength arithmetic on recorded sizes, and the real-parse anchor
   test drives two REAL demo-trace segments (gzipSync'd public/
   demo-trace.bin) through the real core via the SegmentParser seam,
   asserting the accounted rawByteLength IS the actual decompressed
   byte length and resident equals their sum - so 30 MB recorded
   entries behave identically to the anchored ~11 MB real ones.
8. `npm run build`: dist listing IDENTICAL to the T16 record (20 files,
   content-hash changes only; dev-probe chunk byte-identical). The
   worker chunk grows 29.08 -> 30.07 kB (the parse-buffer body path).
   segments.ts IS rollup-transformed (18 -> 19 modules through the
   barrel) and then tree-shaken from the probe output - no page assets
   added, but rollup-only breakage would still surface at build time.
9. `cargo build -p dial9-viewer`: exit 0 (dist content changed;
   rust-embed re-embeds). No cargo nextest / stress / clippy / fmt: no
   .rs touched, no trace-format change (AGENTS.md JS-only rule). No new
   ui-root test_*.js, so no e2e-trace-tests.sh registration (vitest
   auto-discovers src/**/*.test.ts). Local build deletes tracked
   dist/.gitkeep; restored via git checkout before committing (same
   pre-existing quirk as T06-T16).

## DECISIONS A REVIEWER SHOULD SEE

- Reservation-aware eviction (1e3a29c) goes beyond the 2.8 text: the
  spec's completion-time eviction alone lets resident bytes transiently
  exceed the hard budget by one segment while the newcomer parses. The
  DoD ("resident stays under budget") forces the stronger property, so
  reconcile reserves estimates for admitted-but-unparsed work and evicts
  first. The hard clamp remains the backstop for under-estimates.
- Fetch happens on the MAIN thread (orchestrator), parse in the worker.
  T16 moved fetch INTO the worker for whole-trace loads; here the
  orchestrator owns the raw-gzip cache, so bytes must land main-thread
  anyway. The parse-buffer request carries them over (cloned), keeping
  gunzip+parse off the main thread. The T16 load path is untouched.
- One worker per segment parse (T16 lifecycle reused) instead of the
  persistent worker/pool the T16 HANDOFF floated: spawn cost is
  negligible against a segment parse, and every abort/late-message
  guarantee carries over verbatim. The factory seam keeps a pool
  possible without protocol changes (add a jobId then).
- "fetching" covers fetch AND parse phases (T06's 4-state lifecycle is
  frozen; a fifth "parsing" state would be a type break). Per-phase
  progress is observable via onProgress.
- Truncated left-edge polls carry `taskId: null`, not 0: the core's "0 =
  untracked" convention would silently conflate "trace has no task
  tracking" with "the PollStart is outside the window". App-level type,
  explicit null (AGENTS.md no-hidden-absence rule).
- Absolute listing ends are NOT marked truncated: there is no
  "evicted/unfetched" neighbor there (2.8's wording), and the legacy
  whole-trace render drops those polls (#194) - marking them would be a
  parity change owned by a page ticket if ever wanted.
- The gzip cache stores STILL-COMPRESSED bytes (2.8's "raw gzipped
  bytes"); parse-buffer re-gunzips on re-entry. Decompressed buffers are
  never retained (they are not part of any budget's inventory).

## EXPECTED MERGE OVERLAPS (sibling worktrees T12/T18)

- `src/lib/trace/index.ts`: T18 adds its aggregates-client barrel lines;
  this branch adds the segments.ts block. Trivial additive merge; keep
  the explicit named-export style.
- `worker/protocol.ts` / `worker/body.ts`: T18 should not touch these;
  if T12 does, the additions here are one request kind + one handler
  case (additive).
- No package.json / tsconfig / vite.config changes on this branch.

## REMAINING

None for T17. Flagged for chunk 2 (viewer integration):

- Wire setViewport to a store subscription on the viewport slice, and
  boundaryPolls()/computeWindowBoundaryPolls to a store derived() for
  the G5-marker rendering (truncated) and lane rendering (stitched).
- load.ts bootstrap per 2.8 ("list, fetch initial window"): call
  /api/browse, deriveSegmentExtents, mapExtentToMonotonic after the
  first parse, createSegmentWindow with urlFor over /api/object.
- Tier-1 fallback rendering for "listed"/"evicted"/deferred segments
  (T18's aggregates client + listing-metadata density) and the
  "partial data" badge when admission defers need segments.
- Status-bar per-segment progress from onProgress (2.8 feedback edge).
- T42 real encoder fixtures can replace the repeated-demo fixtures in
  the scenario test.

## BLOCKERS

None.
