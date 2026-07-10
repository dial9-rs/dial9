# HANDOFF - fix/T07-store-hardening (T07 audit fix-forward)

(Replaces the T17 HANDOFF inherited through the branch chain; T17's own
record lives in the history at 3004ca2's first-parent chain. Authoritative
spec for this branch: docs/tickets/reviews/T07-audit.md.)

## STATUS

COMPLETE. All five findings from `docs/tickets/reviews/T07-audit.md` are
fixed (1, 2, 3, 5) or documented (4, per the audit's own severity call).
No stop-gates hit: nothing in the review conflicted with the T07
contracts, and finding 2's tsconfig-flag fallout was small (3 sites), so
the stronger type-level option stands - no runtime-strip fallback needed.

Base: integration/chunk-1 @ 3004ca2. No push, no PRs. No .rs, no frozen
core, no page HTML touched.

## COMPLETED (one commit per finding)

- Finding 1: 6088d1b  fix(store): block in-place slice mutation
- Finding 2: 01af6c7  fix(store): reject explicit undefined in update() patches
- Finding 3: b1388ec  fix(store): retry the changed set after a throwing subscriber
- Finding 4: 56c2988  docs(store): note N18 asserts inside-any-flush, not per-subscription
- Finding 5: fd3d943  fix(store): narrow derived() compute to its declared deps

Files touched: `dial9-viewer/ui/src/store/store.ts`,
`dial9-viewer/ui/src/store/store.test.ts`, `dial9-viewer/ui/tsconfig.json`,
plus finding-2 fallout in `src/lib/trace/worker/body.ts`,
`src/types/heatmap.d.ts`, `tests/core/fetch_traces.test.ts`.

## Per-finding summary + before/after evidence

### 1. Shallow Readonly (6088d1b)

Fix: both layers the review recommended. `ReadonlyState<S>` (one-level-deep
readonly, `{ readonly [K]: Readonly<S[K]> }`) on `getState()` and the
`Subscriber` state param; dev-only `Object.freeze` of every slice (initial
slices at createStore, each fresh slice in update()), gated on
`import.meta.env.DEV` like N18 - release builds pay nothing (the branch is
dead-code-eliminated by Vite; the DEV=false path is unit-tested unfrozen).

- Before (store.ts at 3004ca2, new tests in place): runtime test FAILED
  ("expected function to throw an error, but it didn't") and tsc reported
  2x TS2578 unused '@ts-expect-error' at store.test.ts:123,130 - i.e. the
  in-place writes typechecked.
- After: 16/16 store tests pass, tsc clean.

### 2. undefined stamped via update() patches (01af6c7)

Fix: `exactOptionalPropertyTypes: true` in tsconfig.json (the flag-first
option; type-level, stronger). Fallout across the whole ui/ project was
PROPORTIONATE - exactly 3 root sites, all fixed in the same commit:

1. `src/lib/trace/worker/body.ts` - built FetchOptions with
   `headers: request.headers` (possibly undefined); now attaches headers
   conditionally instead of stamping undefined.
2. `src/types/heatmap.d.ts` - SegmentInput optional fields gained explicit
   `| undefined` (faithful: the frozen-core JS treats explicitly-undefined
   like missing, and index.html's normalized segments carry explicit
   undefineds from parseKey; also un-breaks the probe.ts generic
   inference cascade at probe.ts:387/397).
3. `tests/core/fetch_traces.test.ts` - RecordedCall.opts is now
   `FetchOpts | undefined` (records exactly what the mock received).

- Before (flag off): 2x TS2578 unused '@ts-expect-error' at
  store.test.ts:267,269 - `update("viewport", { viewStart: undefined })`
  and `update("selection", { spanFocus: undefined })` both typechecked.
- After: tsc clean project-wide; the store test's legal-omission path
  passes at runtime.
- Residual (accepted): the fix is type-level; an untyped caller could
  still stamp undefined at runtime. The review offered either option;
  type-level chosen per the fix-forward instruction (flag first, fallout
  proportionate).

### 3. Throwing subscriber drops the frame's changed set (b1388ec)

Fix: flush()'s finally now, on abnormal exit, merges the frame's changed
set back into `dirty` (preserving slices the thrower dirtied pre-throw)
and re-arms the scheduler unless a mid-flush update already did - delivery
retries next frame. The error still propagates; the N18 gate still
closes. The existing "gate closed on throwing subscriber" test asserted
only throw-propagation + gate closure (NOT the drop), so it stands
unchanged - no assertion of the buggy behavior existed.

- Before: both new tests FAILED - "expected +0 to be 1" (no tick re-armed
  after the throw) and onBoth called with `{b}` instead of `{a, b}` (the
  restored set was lost; only the thrower's pre-throw update survived).
- After: 19/19 store tests pass, including the no-double-arm case.

### 4. N18 inside-any-flush limitation (56c2988)

Comment-only, at the assertInScheduledRender doc: the module-level counter
proves "inside SOME notification tick", not "fired by YOUR subscription";
a render called directly by an unrelated subscriber passes. N18-clean is
necessary but NOT sufficient - the F2 renderer registry stays the
authority chunk-2 reviewers must check. No behavior change.

### 5. derived() compute sees undeclared slices (fd3d943)

Fix: `derived<T, K extends SliceKey<S>>(deps: readonly K[], compute:
(state: ReadonlyState<Pick<S, K>>) => T)` - undeclared-slice reads are
compile errors. Type-level only, zero runtime cost (the review's Pick
option, chosen over a dev proxy). The existing derived test's compute
annotation narrowed to `Pick<TestState, "a">` to match.

- Before: TS2578 unused '@ts-expect-error' at store.test.ts:356 - the
  undeclared `s.b.s` read typechecked.
- After: tsc clean; 20/20 store tests pass.

## EVIDENCE - gates (final tree)

- `npx tsc --noEmit`: clean (strict + noUncheckedIndexedAccess +
  exactOptionalPropertyTypes).
- FULL `npm run test`: GREEN - 46 files passed | 1 skipped;
  812 passed | 1 expected fail | 11 skipped (824). Baseline before this
  work: 806 passed | 1 expected fail | 11 skipped (818); +6 new tests.
- Coverage (the exact gate command, `npx vitest run --coverage.enabled
  --coverage.include='src/store/**'`, full suite): 100% statements
  (68/68), 100% BRANCHES (28/28), 100% functions (13/13), 100% lines
  (60/60). Baseline was 100% at 54/20/13/48; every new branch (DEV freeze
  x2, abnormal-exit restore, no-double-arm) is covered.
- `npm run build`: succeeds; dist file SET unchanged (22 files, no
  additions or removals). One filename differs by content hash only:
  `dist/assets/trace-worker-DX5-Boi3.js` -> `trace-worker-Xey_3JUz.js`,
  forced by the finding-2 fallout fix inside body.ts (bundled into that
  chunk). No rust-embed shape change.
- store.ts is 247 lines (was 188; the <200 target flexed per the
  fix-forward allowance - the growth is the freeze loop, the finally
  restore block, and finding-4's mandated comment).

## Flake report (AGENTS.md: report apparent flakes, do not ignore)

Two full-suite runs during this session had timeout failures in
DIFFERENT, unrelated frozen-core suites (stream_parse, directory_parse,
slice, load.test, body.test; "Timeout"/"hookTimeout" errors; 2 fails in
one run, 4 in another, disjoint sets). Cause is host contention, not this
change: host load average peaked at 27 (Zoom and other apps at 50%+ CPU),
the same 5 suites pass 77/77 in isolation under that same load, none of
them touch src/store at runtime (and the tsconfig flag has no runtime
effect), and the final full run (load subsided) is fully green in 134s.
No repo timeout settings were changed.

## Open questions

None blocking. Two notes for chunk-2 (T13/T14 subscriber code):

- Finding 2's guard is type-level; if runtime hardening against untyped
  callers is ever wanted, a dev-gated undefined-strip in update() can be
  added without an API change.
- Finding 3's chosen semantics (review-endorsed): subscribers that ran
  before a throw run AGAIN on the retry frame, and a deterministic
  thrower throws once per frame - loud by design. Top-level subscribers
  should still be written not to throw.
