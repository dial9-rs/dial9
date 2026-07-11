# Issue closures: mapping, evidence, and draft close-comments

T44's file (see chunk-3-post.md). One section per issue the migration
resolves: the issue -> ticket mapping, the evidence, and a DRAFT closing
comment. Nothing here is posted to GitHub without maintainer sign-off; the
sign-off convention is the ledger's (maintainer approves the PR that adds or
amends the entry, and only a human or an explicitly-authorized agent posts).

## #593 - Flamegraph search percentage does not match the highlighted frames (T48)

Status: FIXED on ticket/T48-search-count-mismatch (close on land with the
draft comment below). Decision taken: OPTION 1, maintainer-confirmed
against the issue body - the reporter explicitly expects the percentage to
match the highlighted sections ("matched sections clearly have more samples
and more total contribution than the percentage would suggest"). The
percentage is now the highlighted-area share, and the `matchedSelf > 0`
hiding guard is removed. Core-side change in `flamegraph.js` (sanctioned by
T48), so both page generations changed together. Regression suite:
`tests/core/flamegraph_search.test.ts` (failing-first, 14 tests incl. the
demo-trace anchors below). F38 amended `[2026-07-11]` + ledger line added.
The analysis below is kept as the record that produced the decision.

### Root cause

The search stat and the search highlight measure two different things:

- The stat (`updateSearchStats`, `dial9-viewer/ui/flamegraph.js:485-518`)
  computes `matchedSelf / totalSelf`, where `countSearchMatches`
  (`flamegraph.js:106-118`) sums `node.self` over every matching tree node.
  `node.self` counts only samples whose innermost (leaf) frame is that node
  (`trace_analysis.js:1034`), i.e. leaf-attributed "self time".
- The highlight (`flamegraph.js:347-363`, inventory row F40) renders every
  matching frame's full bar at alpha 1.0. A bar's width is
  `node.count / total`, the frame's INCLUSIVE weight (self + descendants).

Mid-stack frames typically have `self == 0` (all of their samples are
attributed to deeper frames), so a query that lights up most of the canvas
reports a tiny percentage. Worse, the `matchedSelf > 0` guard at
`flamegraph.js:513` suppresses the percentage entirely when every matched
frame is mid-stack: the user sees half the flamegraph lit and a stat that
says only "8 frames".

Two adjacent inconsistencies compound the confusion:

1. `frameCount` counts matching TREE NODES, not distinct functions or
   samples: searching `poll` on the 147-sample demo trace reports "146
   frames", and `tokio` reports "230 frames" - more "frames" than samples.
2. The interactive SVG produced by this very page's export button embeds
   flamegraph.pl-style search (`flamegraph_export.js:378-421`, rows
   F79/F84), which reports `Matched: X%` as the union of matched frames'
   x-extents over total width - exactly the highlighted-area share. The page
   and its own export therefore disagree for the same query on the same
   data (measurements below).

### Measurements (demo trace, 147 CPU samples, all worker-side)

Method, fully reproducible:

- Reported stat: drove the real pages via Playwright against the dev server
  (`/new/flamegraph.html?trace=demo-trace.bin` and legacy
  `/flamegraph.html?trace=demo-trace.bin`), typing each query into
  `.fg-search-input` and reading `.fg-search-stats`.
- Highlighted-area share: intercepted `CanvasRenderingContext2D.fillRect`
  in-page during the same runs, classified bars by `globalAlpha` (1.0 = lit,
  0.25 = dimmed per F40), and computed union(lit bar x-extents) /
  union(all bar x-extents) on the worker canvas.
- Cross-checks: (a) a headless Node replication of the page's exact tree
  build (`filterCpuSamples(cpuSamples, null, null)`, split on
  `workerId !== 255`, `buildFlamegraphTree`) computing the union of
  inclusive counts of matched frames with no matched ancestor; (b) the
  page's own SVG export (`treeToInteractiveSvg`) driven via its embedded
  `search()` with ignorecase enabled.

Both page generations (migrated /new and legacy) report byte-identical
stats - they load the same core `flamegraph.js`. In the reported strings
below the separator is the middle-dot character (U+00B7), shown here as `.`:

| Query | Page stat (F38) | Lit-area share (measured from draw calls) | Export SVG `Matched:` (ignorecase) | Headless tree math |
| --- | --- | --- | --- | --- |
| `poll` | `146 frames . 2.7% of samples` | 100.0% | 100% | 100.0% |
| `tokio` | `230 frames . 5.4% of samples` | 100.0% | 100% | 100.0% |
| `axum` | `32 frames` (no pct shown) | 92.5% | 92.5% | 92.5% |
| `dispatcher` | `24 frames` (no pct shown) | 68.6% | 68.7% | 68.7% |
| `framebuf` | `8 frames` (no pct shown) | 54.4% | 54.4% | 54.4% |
| `spawn` | `2 frames` (no pct shown) | 100.0% | 100% | 100.0% |

(The 68.6 vs 68.7 delta is canvas rounding plus the < 0.1%-width node draw
filter; the three independent measurements otherwise agree exactly.)

The mismatch in one line: searching `poll` lights 100% of the canvas and
reports "2.7% of samples"; searching `framebuf` lights 54.4% of the canvas
and reports no percentage at all.

### The two candidate semantics

A. Self-sample share (current F38 math): "what fraction of CPU time is
   spent DIRECTLY inside matching functions" (leaf-attributed). A legitimate
   profiling metric, but it is not what the highlight shows, and the label
   "of samples" does not say "self". If kept, the label must change (e.g.
   "2.7% self") and the `matchedSelf > 0` guard should not silently hide
   the figure for mid-stack matches.

B. Highlighted-area share: "what fraction of samples pass through at least
   one matching frame" = union of inclusive counts of topmost matched
   frames / total. This equals the lit share of the canvas the user is
   looking at, and is the semantic of flamegraph.pl and of this page's own
   SVG export (F79/F84).

Evidence leaning toward B: it matches what the user sees (F40), the
industry convention, and the product's own export surface. A user reading
"X% of samples" next to a highlighted flamegraph most plausibly expects B.
Evidence that keeps A defensible: self time is a real metric and the
cheaper remedy is relabeling, not re-math; the issue author's actual
expectation is the tiebreaker per T48 and was not readable in-environment.

### Decision (was: decision point for the maintainer)

RESOLVED: option 1, maintainer-confirmed against the #593 issue body (the
reporter expects the percentage to match the highlighted sections). The
original three options are kept for the record:

1. Change the math to B (inclusive union share). "X% of samples" stays an
   accurate label; the stat now agrees with the highlight and the export.
   <- CHOSEN.
2. Keep A's math, fix the label ("X% self samples" or similar) and drop the
   asymmetric `matchedSelf > 0` suppression (show `0.0% self` rather than
   nothing). <- rejected: the reporter's expectation is the highlight.
3. Show both (e.g. "54.4% of samples, 0.0% self"). <- rejected: same
   reason, plus toolbar noise.

Implementation (landed on ticket/T48-search-count-mismatch): the edit is in
`flamegraph.js` (`countSearchMatches` returns the union of inclusive counts
of topmost matching frames; `updateSearchStats` uses it and drops the
hiding guard), which BOTH page generations load, so both change together -
intended by the ruling. Regression suite `tests/core/flamegraph_search.test.ts`
(failing-first: 14/14 red pre-fix, 14/14 green post-fix; synthetic
mid-stack/leaf/nested/disjoint cases + the demo-trace anchors above). F38
amended `[2026-07-11]` in features/03 + ledger line. Parity: legacy-vs-new
differ stays zero-diff (same core on both sides); against the OLD baseline
the J5 "searched" checkpoint's `fg.searchStats` readout is the single
expected behavioral delta (e.g. `poll`: "146 frames . 2.7% of samples" ->
"146 frames . 100.0% of samples"); non-search journeys and checkpoints
unaffected (the fix touches only the stat-text path).

### DRAFT closing comment (final; do not post without maintainer sign-off)

> Fixed in <PR link>. Root cause: the search stat counted only SELF samples
> (samples whose leaf frame matched), while the highlight lights every
> matching frame's full (inclusive) bar - mid-stack matches have zero self
> samples, so the number could not line up with the highlight (e.g. on the
> demo trace, searching `poll` highlighted 100% of the canvas but reported
> "2.7% of samples", and some queries highlighted half the graph while
> showing no percentage at all).
>
> The percentage now reports the share of samples that pass through at
> least one matching frame: the union of the highlighted frames' extents
> (a match nested under another match adds no new area) over the samples
> in view - the same semantic as the SVG export's "Matched: X%" and
> flamegraph.pl, so the number now matches what is highlighted. It is also
> no longer hidden when all matches are mid-stack. On the demo trace,
> `poll` now reports 100.0% (was 2.7%), and `framebuf` reports 54.4%
> (previously no percentage at all) - both equal to the measured lit share
> of the canvas.
>
> Verified with a failing-first regression suite over the search-stat math
> (synthetic trees + demo-trace anchors) and a behavioral-parity run
> showing the stat as the only delta.
