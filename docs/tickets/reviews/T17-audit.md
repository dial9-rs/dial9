# T17 audit - segment-windowed loading (integration/chunk-1 @ 3004ca2)

Scope: `dial9-viewer/ui/src/lib/trace/segments.ts`, `segments.test.ts`,
`segments.window.test.ts`, `src/types/state.d.ts`, `src/types/trace.d.ts`
on `integration/chunk-1` (tip 3004ca2, merge of `ticket/T17-segment-windowing`).
Authorities: chunk-1 ticket T17 + SHARED DECISIONS budget block;
`02-architecture.md` 2.8 + N19; features/02 G5. Correctness only.

Method: full read of implementation + both suites; suite run
(`npx vitest run src/lib/trace/segments.test.ts src/lib/trace/segments.window.test.ts`
in the INTEG worktree: 60/60 pass); two executable probes against the
public API for findings 1 and 2 (temporary test file, run, then removed -
results quoted below).

## Verdict: BLOCKING

Findings 1 and 2 are user-visible wrong-diagnostics / unusable-detail
defects in exactly the scenarios T17 exists for (long-blocked polls;
heavy traces). 3-5 are real but narrower. 6-7 are notes, not defects.

## Findings

1. **BLOCKING - a poll spanning 3+ segments vanishes, even with every
   segment resident.**
   `segments.ts:634-716` (stitching is pairwise-adjacent only) +
   `segments.ts:531-584` (a worker silent for a whole segment produces no
   edge evidence). A poll whose PollStart is in segment k and PollEnd in
   segment k+2 has no `closeAtStart` in k+1 (unmatched openAtEnd dropped
   at the k/k+1 boundary, `segments.ts:664`) and no `openAtEnd` in k+1
   (unmatched closeAtStart dropped at k+1/k+2). Verified by execution:
   `computeWindowBoundaryPolls` over three PARSED segments
   (`pollStart(10,w1)` in s0, only worker-2 events in s1, `pollEnd(210,w1)`
   in s2) returns `{"truncated":[],"stitched":[]}`.
   Failure scenario: a worker blocked in one poll for minutes - the
   pathology this tool diagnoses - renders as IDLE across every interior
   segment; the legacy whole-trace load renders the same data as one long
   poll, so this is also a direct parity break, not just an edge case.
   Same root cause makes the "middle segment resident, both neighbors
   evicted" variant fully invisible too (eviction drops `edgePolls`,
   `segments.ts:1211`, discarding the neighbor evidence that could flag
   "worker w is mid-poll through this entire segment").

2. **BLOCKING - a segment whose decompressed size exceeds the hard budget
   enters a permanent parse -> evict loop and is never viewable.**
   `segments.ts:322-325` (capToBudget force-admits the nearest first-round
   candidate regardless of size, per N19 "minimum useful window is one
   segment") vs `segments.ts:427-441` (planEviction's hard clamp evicts
   need members with NO matching minimum-window exception). The comment at
   `segments.ts:1218-1222` claims that after the first eviction "admission
   defers it honestly" - false for the nearest candidate, which the
   force-admit clause re-admits every pass. Verified by execution: one
   segment, `rawByteLength` 2000 vs budget 1000 - entry state is `evicted`
   after EVERY `setViewport`, `parsesStarted` increments per call,
   `peakResidentRawBytes` = 2000 (2x budget).
   Failure scenario: at the motivating scale (~100 MB/min gzip, ~3-4x
   expansion) a segment over ~40 MB gzip decompresses past 128 MiB; chunk 2
   wires `setViewport` to the viewport slice, so every pan/zoom tick
   re-parses ~150+ MB (~5 s worker CPU at the documented ~33 MB/s) and
   immediately evicts it - detail view permanently unavailable, resident
   heap spiking to the full oversized parse each time.

3. **MEDIUM - stitching trusts listing adjacency, not extent adjacency:
   holes in the listing fabricate completed long polls.**
   `segments.ts:657-674` stitches across any two CONSECUTIVELY LISTED
   parsed segments; `orderedKeys` excludes entries skipped by
   `deriveSegmentExtents` (`segments.ts:169-174`) and anything missing
   from the listing (retention-deleted object). If segment B between A and
   C is absent, A.openAtEnd(w) and C.closeAtStart(w) - evidence from two
   DIFFERENT polls, the first ending and the second starting inside B -
   stitch into one fabricated complete poll spanning the missing data:
   precisely the "completed long poll" lie the 2.8 hard edge forbids. An
   extent-gap guard (A.extent.endNs vs C.extent.startNs) would catch it;
   genuine gaps survive tiling (`segments.ts:189-198`) so the signal exists.

4. **MEDIUM - stale idle prefetch bypasses budget admission.**
   `segments.ts:1120-1132`: the idle callback re-verifies GEOMETRIC
   membership (`computePrefetchSet`) but never re-runs `capToBudget`. An
   idle queued under viewport V1 (key admitted) can fire under V2 where
   the key is still a geometric neighbor but was budget-DEFERRED by the
   latest reconcile; `startJob` runs anyway. The resulting parse is
   unprotected in `planEviction` (it is outside `prefetchPlan.admitted`),
   so it is evicted at the completion pass - after transiently pushing
   resident past the trigger (and potentially past the hard budget) and
   wasting a fetch + parse.
   Failure scenario: rapid panning with a full need set - each stale idle
   burns a network fetch and a worker parse whose output is discarded.

5. **LOW - the "resident <= budget throughout" invariant is
   estimate-dependent, and the DoD test cannot see the failure mode.**
   Reservations use `GZIP_EXPANSION_ESTIMATE = 4` (`segments.ts:84-90`,
   consumed at `segments.ts:1188-1193`); a first-time segment expanding
   more than 4x lands over the trigger (up to over the hard budget) before
   the completion-pass eviction runs (`segments.ts:1103-1106` records the
   over-budget peak first). Acknowledged in-code as a "backstop", but the
   10-segment DoD scenario pins expansion at 3x
   (`segments.window.test.ts:643-646`), so its
   `peakResidentRawBytes <= budget` assertions can never exercise
   under-estimation. A >4x fixture (highly repetitive event streams gzip
   well past 4x) would fail the "throughout" claim today.

6. **NOTE (not a defect) - truncation output is produced but consumed
   nowhere.** `SegmentWindow.boundaryPolls()` (`segments.ts:1242`) and the
   pure functions (exported via `lib/trace/index.ts:90`) are the only
   surface; no renderer reads `truncatedAt`/`openEnded` yet. Expected for
   T17 (chunk 2 owns the G5 marker rendering), but the lie-prevention is
   not end-to-end until then - the chunk-2 review must confirm a consumer.

7. **NOTE - hard-clamp eviction of a need-set segment is silent.**
   `segments.ts:427-441` + `:1203-1216`: a need segment dropped by the
   hard clamp flips to `evicted` with no log/onError/badge signal.
   Acceptable under 2.8's tier-1 fallback ONLY if chunk 2 renders the
   partial-data state; 2.8's "never silently wrong" makes this a chunk-2
   gate, flagged here so it is not lost.

## Audit points verified clean

- **Boundary-poll truncation, 2-segment cases (both directions):**
  start-in-evicted/end-in-resident surfaces as `truncatedAt: "start"` with
  `taskId: null` (task identity honestly unknown); start-in-resident/
  end-in-unfetched surfaces as `truncatedAt: "end"` clamped to observed
  maxTs - a lower bound, never inflated (`segments.ts:676-711`; pinned by
  `segments.test.ts:548-596` and the orchestrator test at
  `segments.window.test.ts:475-531` with exact-object assertions). The
  clamps (`min(minTs, close.end)` / `max(maxTs, open.start)`) are sound
  lower bounds since the crossing poll is provably in flight across the
  segment's observed range.
- **Cancellation:** AbortController signal reaches the fetcher AND
  `parseJob.abort()` (`segments.ts:1029-1039`); the fetch-resolved-but-
  parse-not-yet-created race is handled (`segments.ts:1083-1087`); a
  parse that completes after abort never writes the store
  (`segments.ts:1089`); both phases pinned with late-resolution tests
  (`segments.window.test.ts:279-341`). An aborted fetch's bytes are
  discarded before `cache.set` (minor wasted download, not correctness).
- **Two-level cache coherence:** the mock-fetch-count test is real
  (per-URL call counts + `parsesStarted` + `cacheHits`,
  `segments.window.test.ts:347-410`); the raw cache is per-window and
  keyed by the same segment-key string as the store slice, `urlFor` maps
  to URL only at fetch time, so no S3-key vs `trace=` collision is
  constructible within an instance.
- **Eviction distance + hysteresis:** `extentDistance` handles a viewport
  between two resident ranges symmetrically; trigger =
  `floor(0.9 * budget)`, fires strictly above it (== trigger does not
  evict, pinned at `segments.test.ts:264-277`); all arithmetic in bytes,
  no unit mixing; constants match the shared decision (128 MiB / 256 MiB
  / 0.9).
- **Need set zoomed out wider than all segments:** returns all segments;
  admission caps nearest-center-first, remainder deferred to tier 1 -
  correct N19 behavior (no explicit test for this shape, but the pure
  functions make it derivable; low risk).
- **Budget accounting:** `recomputeResident` recomputes from the store on
  every change rather than keeping incremental counters
  (`segments.ts:1000-1010`) - no drift possible across abort/evict/
  re-parse paths. Peak stat honestly records overshoots (see finding 5).
- **Test honesty:** 60/60 pass; each of the four client-side hard edges
  has a real, non-tautological test; the 10-segment scenario asserts
  resident <= 128 MiB after every step in both directions plus
  peak <= budget (peak samples every recompute point, so this covers
  intra-step) and zero re-downloads on the back pass - within the limits
  of finding 5's fixed 3x expansion and finding 1's untested 3-segment
  span.

## Required before chunk-2 integration

- Fix 1: carry cross-segment poll continuity - retain `edgePolls` across
  eviction (they are tiny) and let the stitcher span silent interior
  segments when the worker has no lifecycle events there, or match
  openAtEnd/closeAtStart across runs with explicit truncation at real
  window edges. Add the 3-segment-span test (all-resident AND
  middle-only-resident variants).
- Fix 2: give `planEviction` the same minimum-window exception
  `capToBudget` has (never hard-clamp the single nearest need segment),
  or make admission genuinely defer oversized segments after the first
  real size is learned - either way, kill the loop and add an
  oversized-single-segment test.
- Fix 3: extent-adjacency guard on internal stitching.
- Fix 4: re-run admission (or at least a budget check) inside the idle
  callback.
- Fix 5: add a >4x-expansion fixture to the 10-segment scenario, or
  soften the DoD claim to match the backstop design.
