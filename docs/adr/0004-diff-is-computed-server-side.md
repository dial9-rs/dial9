# A/B diff and significant-difference ranking are computed server-side

**Status:** accepted

The unified diff-first view compares two selections (A and B) across every
view (flamegraph, span-stats, tokio). We compute the merge and the ranked
"significant differences" **server-side** — one endpoint resolves both
selections, schedules fold work across both to keep coverage comparable, and
streams an already-merged diff
structure plus a bounded ranked-difference list — rather than shipping both
full structures to the browser and merging in JS (as today's flamegraph-only
`flamegraph_diff.js` does).

## Why

- The server already holds the folded data in the right shape (the fold
  accumulators are resident over DuckDB/Parquet). Diffing there is a diff over
  existing structures; client-side means shipping both full structures and
  rebuilding the merge in JS.
- The generalized-span design deliberately **bounds** returned output ("do not
  return every instance for fleet scopes"). Client-side ranking can only rank
  what was shipped, reintroducing a sampling bias the server does not have.
- The honest end-state for significance — per-node/per-type confidence
  intervals that tighten as files fold — requires per-file sample statistics
  that live server-side.
- The endpoint **schedules** fold work across both sides to keep their coverage
  comparable (not rigid lockstep — they may drift within a tolerance), which is
  what makes the difference ranking trustworthy under demand-driven partial
  folds.
- One Rust diff+rank implementation over the kind-agnostic span accumulator
  serves every view, versus re-implementing merge+rank per view in JS.

## Consequences

- A new server endpoint (or a `compare=` mode on the existing per-view
  endpoints) owns both scopes.
- The client renders the two synced panels + listing table and owns only
  interactive navigation (zoom/search/inspect) over the shipped structure.
- The existing unit-tested `flamegraph_diff.js` client merge is superseded —
  kept, if at all, as a thin fallback.
