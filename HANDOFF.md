# FIX-T17 HANDOFF - segment-windowing audit fixes

(Replaces the T17 HANDOFF inherited through the branch chain; T17's own
record lives at commit e29c258.)

Branch `fix/T17-segment-stitching-budget`, based on the integrated
chunk-1 tip 3004ca2. Authoritative spec: the adversarial audit at
`docs/tickets/reviews/T17-audit.md` (findings 1-5; 1 and 2 BLOCKING).

## STATUS

DONE - both blocking findings fixed, both medium findings fixed, and the
low-severity test-coverage item covered; every audit probe is now a
permanent, named regression test (failing-first verified for all four
findings). All gates pass (evidence below). No STOP-gate hit: the
N-segment stitching design raised no N19 memory conflict - retained edge
evidence is bounded at one open + one close per worker per segment, and
chain assembly is pure derivation with no retained buffers. Scope fence
respected: no .rs edits, no frozen-core .js, no page HTML, no new dist
assets, no pushes/PRs.

## COMPLETED (commits on top of 3004ca2)

- `c02cc0b` findings 1 (BLOCKING) + 3 (medium), one coherent rewrite:
  `computeWindowBoundaryPolls` becomes a per-worker chain walk. An open
  poll is carried across consecutively listed segments while (a) their
  extents are ADJACENT (finding 3: no unobserved time between them; a
  listing hole yields truncated fragments on both sides, never a
  fabricated complete poll) and (b) the worker is provably SILENT there
  (via the eviction-surviving invariants worker set - a PollEnd, Park or
  PollStart would have closed the poll). Fully-resident chains stitch
  into ONE complete poll from k >= 2 fragments; partially-resident
  chains surface each maximal resident stretch as an explicitly
  truncated WindowEdgePoll - a prefix-resident chain truncates at the
  last resident edge. edgePolls are now RETAINED across eviction (they
  are tiny), which makes the audit's middle-only-resident variant
  visible as a "both"-truncated poll carrying its real task identity.
  Types additive: `WindowEdgePoll.truncatedAt` gains "both".
- `bb97d5c` finding 2 (BLOCKING): a segment whose real decompressed size
  exceeds the resident budget now parses exactly ONCE and moves to the
  new terminal `"oversized"` lifecycle state (additive union member; the
  T06 exhaustive-switch compile gate updated with it) instead of
  spinning through force-admit -> parse -> hard-clamp-evict forever. The
  parse is dropped at completion, so it never enters resident accounting
  (no 2x-budget peak); rawByteLength, invariants and edgePolls are kept,
  so lane stability and boundary-poll continuity behave exactly as
  across an eviction. reconcile filters oversized keys from admission
  AFTER computing geometry (an oversized need segment still contributes
  its neighbors to prefetch geometry); startJob refuses them as defense
  in depth. The misleading comment ("admission defers it honestly") is
  corrected to describe the two real deferral shapes. Consumers
  distinguish "too big for budget" (oversized) from "not yet fetched"
  (listed) by state.
- `f972c2a` finding 4 (medium): the admission pipeline (geometry ->
  oversized filter -> need round -> prefetch round) is extracted into
  `planAdmission` (pure extraction; reconcile behavior unchanged) and
  the idle prefetch callback now re-runs ADMISSION, not just geometry -
  a stale idle whose key was budget-deferred by the latest reconcile
  starts nothing.
- (final commit) this HANDOFF.

## EVIDENCE (per finding: before -> after)

All runs in `dial9-viewer/ui` (npm ci done) on this branch.

- Finding 1 (audit probe: 3-segment poll, all segments resident,
  returned `{truncated: [], stitched: []}`): the regression tests were
  written first and run against the unfixed tree - 5 pure failures
  ("stitches a poll spanning 3 resident segments", "TWO silent interior
  segments", "prefix-resident chain", "middle-only-resident",
  extent-gap guard) plus 2 orchestrator failures (the eviction-retention
  pin and "worker mid-poll through the only-resident segment"). After
  `c02cc0b`: all pass; the probe fixture returns the single stitched
  complete poll {start: 10, end: 210, taskId: 7}.
- Finding 2 (audit probe: rawByteLength 2000 vs budget 1000 - state
  "evicted" after EVERY setViewport, parsesStarted incrementing per
  call, peakResidentRawBytes 2000): the three tests in "T17-audit
  finding 2: oversized segments defer honestly" failed before the fix;
  after `bb97d5c` the probe fixture settles at state "oversized" with
  networkFetches 1 and parsesStarted 1 across 5 viewport ticks,
  evictions 0, residentRawBytes 0, peakResidentRawBytes 0.
- Finding 3 (fabricated stitch across a listing hole): "never stitches
  across an extent gap" failed before `c02cc0b` (the fabricated
  {start: 10, end: 310} completed poll was produced); now both
  fragments surface truncated ("end" with task identity, "start" with
  taskId null) and stitched is empty.
- Finding 4 (stale idle bypasses admission): "T17-audit finding 4: a
  stale idle callback re-runs BUDGET admission, not just geometry"
  failed before `f972c2a` with the audit's exact waste (the
  budget-deferred key was fetched: "expected [ ...(3) ] to not include
  ...segKey(1)"); now zero fetch/parse for the deferred key and
  networkFetches stays at 2.
- Low item (the 10-segment DoD scenario pins expansion at exactly 3x,
  blind to the finding-2/-5 class): covered by the NEW scenario
  "10-segment walk with one oversized segment" - one 15x expander among
  3x segments; one parse total for it across an out-and-back walk,
  resident and peak <= budget after every step, zero re-downloads. It
  failed before `bb97d5c` (the walk looped re-parsing the oversized
  segment). The original DoD scenario is deliberately untouched; see
  DECISIONS.

## REGRESSION TESTS (named so the audit can find them)

In `src/lib/trace/segments.test.ts`:
- `computeWindowBoundaryPolls: T17-audit finding 1 (N-segment stitching)`
  (the 3-segment probe, 4-segment span, prefix-resident truncation,
  middle-only-resident, unmatched-chain whole-drop parity)
- `computeWindowBoundaryPolls: T17-audit finding 3 (extent-gap guard)`
  (gap fragments; touching/tiled extents remain adjacent)

In `src/lib/trace/segments.window.test.ts`:
- `T17-audit finding 2: oversized segments defer honestly` (the audit
  probe, oversized-vs-listed distinguishability, the oversized
  10-segment scenario)
- `T17-audit finding 1: poll continuity across evicted neighbors`
- `T17-audit finding 4: a stale idle callback re-runs BUDGET admission,
  not just geometry` (inside the prefetch describe)

## DECISIONS A REVIEWER SHOULD SEE

- STITCHED requires a fully-resident chain (open, close and every
  interior parsed). Evidence retained on evicted/oversized entries
  (edgePolls, invariants) informs CONTINUITY and task identity but
  never contributes rendered spans - the audit-verified-clean
  two-segment truncation semantics (start-in-evicted surfaces
  truncated, never stitched) are preserved exactly.
- Truncated spans stay clamped to RESIDENT observed events. Retained
  neighbor evidence upgrades `taskId` from null to the real id when the
  PollStart was ever parsed; the type doc now says null means "genuinely
  unknown" (never parsed AND no retained evidence).
- `truncatedAt: "both"` is the honest vocabulary for a poll provably in
  flight through entire resident segments (start and end both beyond
  the window). Additive union extension; no consumer exists yet (audit
  note 6 - chunk 2 owns rendering).
- A chain meeting a worker-ACTIVE boundary with retained edges but no
  matching counterpart DROPS WHOLE (core #194 parity): the silence
  proof is only as good as the wire, so a lost close must not resurrect
  the interior stretches. Pinned by the "dropped whole" test.
- "oversized" is entered ONLY on a learned REAL size (parse
  completion), never on estimates: N19's first-round force-admit still
  gives every segment its one real fetch + parse (a window limit, not a
  load-time rejection on a guess). The residual N19 tension is
  recorded: a single oversized need segment is honestly unavailable at
  this budget - the tier-1 badge rendering is a chunk-2 gate (audit
  note 7).
- The audit offered a planEviction min-window exception as the
  alternative finding-2 fix (keep the oversized segment resident at a
  ~2x-budget cost). The honest-deferral option was chosen per the
  fix-forward mandate (explicit state, distinguishable from
  not-yet-fetched, no 2x resident peak). planEviction is untouched.
- The low item said "loosen the fixture"; instead the original
  10-segment DoD scenario stays byte-identical and a SIBLING scenario
  with one oversized segment was added - the finding-2/-5 class is
  covered without rewriting the pinned DoD record. Finding 5 proper
  (>4x-but-still-fitting expanders can transiently overshoot before the
  completion pass) remains the documented backstop design; the audit
  listed 5 as LOW with "or soften the DoD claim" as an accepted outcome.
- Assertions updated where they pinned buggy behavior (each called out
  in its commit message): the two-level-cache test's "edgePolls
  undefined after eviction" pinned the finding-1 defect and now pins
  retention; `exhaustive.test.ts` grew the "oversized" switch arm - the
  T06 compile gate working as intended on a union extension, not a
  pinned bug.
- The mock-parser harness gained an additive per-segment
  `rawByteLengthFor` hook (finding 2/4 fixtures need heterogeneous
  sizes); existing callers are unaffected.

## GATES

1. `npx tsc --noEmit`: clean (exit 0), re-verified after each commit.
2. `npm run test` (full suite, run alone): 46 files passed, 1 skipped;
   818 tests passed, 1 expected fail, 11 skipped; exit 0. (One earlier
   run executed CONCURRENTLY with a targeted vitest invocation timed out
   in tests/core/slice.test.ts at its 60s limit - pure CPU contention
   from the double demo-trace parse; it does not reproduce in the clean
   gate run above.)
3. `npm run build`: dist listing UNCHANGED vs the T17 record (22 paths
   incl. .gitkeep; worker chunk trace-worker 30.07 kB, dev-probe
   37.80 kB). Verified stronger than required: a scratch worktree built
   at base 3004ca2 with the same node_modules produced a dist tree that
   is BYTE-IDENTICAL (diff -r) to this branch's - segments.ts changes
   are tree-shaken from the probe output and the worker body is
   untouched, so even the content hashes match.
4. `cargo build -p dial9-viewer`: exit 0. No cargo nextest / stress /
   clippy / fmt: no .rs touched, no trace-format change (AGENTS.md
   JS-only rule). No new ui-root plain-script tests, so no
   e2e-trace-tests.sh registration (vitest auto-discovers
   src/**/*.test.ts).

## REMAINING (chunk-2 obligations, now with new vocabulary)

- Chunk 2 renders: the "oversized" badge + tier-1 fallback (audit
  note 7), `truncatedAt: "both"` G5 markers, and a real consumer for
  `boundaryPolls()` (audit note 6).
- Future SegmentLifecycle consumers must handle "oversized"; the T06
  exhaustive-switch gate enforces it at compile time.

## BLOCKERS

None.
