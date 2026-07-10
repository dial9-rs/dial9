# T48 HANDOFF - flamegraph search-count mismatch (#593)

(Replaces the T19 HANDOFF inherited through the branch chain; T19's record
lives at commit e6b95a6.)

## STATUS

BLOCKED-ON-MAINTAINER (semantic choice) - the expected STOP-gate for this
ticket's analysis-first mode. Root cause is proven and measured; the fix is
a small core-side edit, but T48 ties the choice of percentage semantic to
the #593 issue author's expectation, which is not readable from this
environment (GitHub access forbidden). In-repo evidence leans strongly one
way but does not make the alternative indefensible, so no code was changed.

Branch: `ticket/T48-search-count-mismatch`, based on integrated tip 74bab07.

## COMPLETED (commits)

| sha | what |
| --- | --- |
| be77d29 | `docs/tickets/issue-closures.md` (T44's file, created): #593 entry with root cause, three-way-cross-checked measurements, both candidate semantics, maintainer decision point, DRAFT closing comment |

Analysis-only: no code gates required; doc integrity only. The measurement
dev server (port 3121) was killed and verified down.

## ROOT CAUSE

The stat and the highlight measure different things:

- `updateSearchStats` (`dial9-viewer/ui/flamegraph.js:485-518`) reports
  `matchedSelf / totalSelf`; `countSearchMatches` (`:106-118`) sums
  `node.self`, which counts only samples whose innermost (leaf) frame is
  that node (`trace_analysis.js:1034`).
- The highlight (`:347-363`, row F40) lights every matching frame's FULL
  bar; bar width is `node.count / total`, the INCLUSIVE weight.

Mid-stack frames have `self == 0`, so a query lighting most of the canvas
reports a tiny number, and the `matchedSelf > 0` guard (`:513`) hides the
percentage entirely when every match is mid-stack. Compounding evidence:
the page's own SVG export embeds flamegraph.pl-style search
(`flamegraph_export.js:378-421`, rows F79/F84) whose `Matched: X%` IS the
highlighted-area share - the page disagrees with its own export on the same
data. Also, "N frames" counts matching tree nodes, so it can exceed the
sample count.

## EVIDENCE (measured numbers)

Demo trace, 147 CPU samples (all worker-side; offworker empty). Reported
stat read live from `.fg-search-stats` via Playwright on BOTH
`/new/flamegraph.html?trace=demo-trace.bin` and legacy
`/flamegraph.html?trace=demo-trace.bin` over the dev server - byte-identical
on both generations (same core flamegraph.js). Lit-area share measured
exactly by intercepting canvas `fillRect` during the same runs: union of
alpha-1.0 bar x-extents / union of all bar x-extents (F40 dims non-matches
to 0.25). Cross-checked two independent ways: headless Node replication of
the page's tree build (union of inclusive counts of topmost matched
frames), and the export SVG (`treeToInteractiveSvg`) driven through its
embedded `search()` with ignorecase.

| Query | Page stat (F38) | Lit-area (draw calls) | Export SVG `Matched:` | Tree math |
| --- | --- | --- | --- | --- |
| poll | 146 frames, 2.7% of samples | 100.0% | 100% | 100.0% |
| tokio | 230 frames, 5.4% of samples | 100.0% | 100% | 100.0% |
| axum | 32 frames (no pct) | 92.5% | 92.5% | 92.5% |
| dispatcher | 24 frames (no pct) | 68.6% | 68.7% | 68.7% |
| framebuf | 8 frames (no pct) | 54.4% | 54.4% | 54.4% |
| spawn | 2 frames (no pct) | 100.0% | 100% | 100.0% |

(68.6 vs 68.7: canvas rounding + the sub-0.1%-width draw filter; the
independent measurements otherwise agree exactly.)

One-line version: searching `poll` lights 100% of the canvas and reports
"2.7% of samples"; `framebuf` lights 54.4% and reports no percentage at all.

## THE QUESTION FOR THE MAINTAINER

With the #593 issue body in hand, pick the remedy (full write-up + draft
closing comment in `docs/tickets/issue-closures.md`):

1. Change the math to the highlighted-area share: samples passing through
   at least one matching frame (union of inclusive counts of topmost
   matched frames / total). Matches the highlight, the SVG export, and
   flamegraph.pl; "X% of samples" remains an accurate label.
2. Keep the self-sample math but relabel ("X% self samples") and stop
   suppressing the figure when matchedSelf == 0.
3. Show both.

Evidence leaning: (1) - it is what users see (F40), the industry
convention, and this product's own export semantic (F79/F84). But (2) is a
defensible cheaper remedy (self time is a real metric; only the label
lies), so the choice is not made unilaterally per the ticket.

## IMPLEMENTATION NOTES FOR THE FIX (once decided)

- Sanctioned core-side edit: `countSearchMatches` / `updateSearchStats` in
  `flamegraph.js`. BOTH page generations load this file, so both change
  together.
- Regression test in `dial9-viewer/ui/tests/core/` (the flamegraph
  export/search suites show the pattern); gates: `npx tsc --noEmit`, FULL
  `npm run test`, `npm run build`.
- Amend rows F38/F40 in
  `docs/ui-inventory/features/03-flamegraph-html.md` in-diff + ledger line.
- Byte-parity implications: re-run the census/behavioral differ baselines.
  J5's "searched" checkpoint captures `.fg-search-stats` (readout
  `fg.searchStats`, `parity/fixtures/readout-schema.mjs:61`), so the stat
  change must appear there as the ONLY behavioral delta; J5 must stay
  zero-diff for non-search steps and all non-search journeys zero-diff.

## OPEN QUESTIONS

- The semantic choice above (blocking).
- If option 1 is chosen: should "N frames" change too? It counts matching
  tree nodes (146 "frames" for `poll` on a 147-sample trace; 230 for
  `tokio`). Counting distinct matched function names, or topmost matched
  extents, would both read saner; flag in the fix PR either way.

## SCOPE FENCE

- Frozen core untouched; no inventory rows amended; no ledger line (none
  earned - analysis only, per ledger format that records row changes).
- No push, no PRs, no GitHub access.
- Scratch measurement scripts were not committed (methodology recorded in
  issue-closures.md precisely enough to reproduce).
