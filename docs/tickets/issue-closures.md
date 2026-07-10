# Issue closures: mapping, evidence, and draft close-comments

T44's file (see chunk-3-post.md). One section per issue the migration
resolves: the issue -> ticket mapping, the evidence, and a DRAFT closing
comment. Nothing here is posted to GitHub without maintainer sign-off; the
sign-off convention is the ledger's (maintainer approves the PR that adds or
amends the entry, and only a human or an explicitly-authorized agent posts).

## #593 - Flamegraph search percentage does not match the highlighted frames (T48)

Status: analysis complete; fix BLOCKED on a maintainer semantic decision
(see "Decision point" below). Analyzed in ticket/T48-search-count-mismatch
without GitHub access, so the issue body's exact expectation could not be
read; the decision point is written so the maintainer can answer it with the
issue body in hand.

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

### Decision point (maintainer)

Answer with the #593 issue body in hand - which remedy matches the report:

1. Change the math to B (inclusive union share). "X% of samples" stays an
   accurate label; the stat now agrees with the highlight and the export.
2. Keep A's math, fix the label ("X% self samples" or similar) and drop the
   asymmetric `matchedSelf > 0` suppression (show `0.0% self` rather than
   nothing).
3. Show both (e.g. "54.4% of samples, 0.0% self").

Implementation notes for whichever branch is chosen (sanctioned core-side
change per T48): the edit lands in `flamegraph.js`
(`countSearchMatches`/`updateSearchStats`), which BOTH page generations
load, so both change together; it needs a `tests/core/` regression suite
(the flamegraph export/search suites show the pattern), amended F38 (and
F40 cross-reference) rows in `docs/ui-inventory/features/03-flamegraph-html.md`
plus a ledger line, and a re-run of the census/behavioral differ baselines:
J5's "searched" checkpoint captures `.fg-search-stats` (readout
`fg.searchStats`, `parity/fixtures/readout-schema.mjs:61`), so the stat
change must appear there as the ONLY behavioral delta, with all non-search
journeys still zero-diff.

### DRAFT closing comment (do not post without maintainer sign-off)

> Fixed in <PR link>. Root cause: the search stat counted only SELF samples
> (samples whose leaf frame matched), while the highlight lights every
> matching frame's full (inclusive) bar - mid-stack matches have zero self
> samples, so the number could not line up with the highlight (e.g. on the
> demo trace, searching `poll` highlighted 100% of the canvas but reported
> "2.7% of samples", and some queries highlighted half the graph while
> showing no percentage at all).
>
> [Variant if option 1 is chosen:] The percentage now reports the share of
> samples that pass through at least one matching frame (the union of the
> highlighted frames' extents) - the same semantic as the SVG export's
> "Matched: X%" and flamegraph.pl, so the number now matches what is
> highlighted.
>
> [Variant if option 2 is chosen:] The stat is now labeled as self time
> ("X% self samples") so it no longer claims to describe the highlighted
> area, and it is no longer hidden when matches are mid-stack.
>
> Verified with a regression test over the search-stat math and a
> behavioral-parity run showing the stat as the only delta.
