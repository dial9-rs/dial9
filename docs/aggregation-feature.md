# Aggregated Flamegraphs — Feature Summary

*Status: implemented (merged June 2026). This is the "what exists today and how
it works" summary. For the canonical vocabulary see [`CONTEXT.md`](../CONTEXT.md);
for the storage decision see [ADR-0003](adr/0003-folded-set-is-the-output-listing.md);
for the longer design narrative see [`aggregator.md`](../aggregator.md).*

## What it does

Builds CPU flamegraphs over a fleet's worth of dial9 traces — a time range
across many hosts — without aggregating every file first. You pick a scope
(time window + hosts), and the viewer shows a flamegraph **immediately** from a
small representative sample, then **progressively refines** it as it folds more
files in the background of successive polls. A coverage label always tells you
how complete the current view is.

The key insight: most of the information in 24h of profiling data is in the
first few samples. A small, *representative* spread of files approximates the
whole window, so we deliberately never fold all of it.

## The pivot

The original *design* was **batch**: decode every trace segment under a prefix
into a Parquet `samples` table up front, and have the viewer query whatever had
been pre-aggregated.

The shipped feature is instead **demand-driven, progressively-refined
sampling**, driven by the query itself:

| | Batch (original design) | Demand-driven (shipped) |
|---|---|---|
| When aggregation happens | Ahead of time, whole window | At query time, a few files per poll |
| How much | Everything | A representative sample, capped (~5%) |
| First result | After the whole batch | After the baseline (4 files), sub-second |
| Completeness signal | None | `coverage` on every response |
| `samples` table | Same schema — one row per CPU sample, drilled into at query time. Now populated *incrementally and partially* instead of batch-filled. |

There is **no `dial9 ingest` batch command** — aggregation happens only inside
the `/api/flamegraph` refinement loop. Nothing pre-aggregates ahead of a query.

## How it works

### 1. Order
Files are ordered by `order_key = BLAKE3(ORDER_VERSION ++ source_key)`,
ascending. This is a deterministic but *pseudo-random* permutation — uniform
across host and time — so the first K files are a representative spread of the
scope rather than one host's earliest minutes. The order is recomputed at query
time and persists nothing.

### 2. Fold
A **fold** decodes one source file's CPU samples and writes them as a
deterministically-named Parquet part-file:

```
{output_prefix}/v{SAMPLES_FORMAT_VERSION}/bucket={source_bucket}/samples/service=…/date=…/host=…/{blake3(source_key)}.parquet
{output_prefix}/v{SAMPLES_FORMAT_VERSION}/bucket={source_bucket}/dict/stacks/{blake3(source_key)}.parquet
{output_prefix}/v{SAMPLES_FORMAT_VERSION}/bucket={source_bucket}/polls/{blake3(source_key)}.parquet
```

The part-file's **existence is the record that the file is folded** — there is
no manifest and no skip-set (see ADR-0003). A zero-sample file still writes an
empty part-file, so it is never re-fetched. Re-folding writes the same key, so
folding is **idempotent**.

### 3. Refinement loop (stateless, per-poll)
Each `GET /api/flamegraph` request:

1. **Matched set** — lists the source scope, filters to the [scope]'s
   service / host-set / time (interval-overlap on the filename `epoch`, padded
   by the segment duration), and sorts by the order key.
2. **Folded set** — lists the output `samples/` tree (the record of what's
   already folded).
3. **Fold a bounded budget** of not-yet-folded files in order — the
   *baseline floor* (4) on the first refining poll, a *refine batch* (12)
   afterward — stopping at the *sampling cap*.
4. **Aggregate** the folded-in-scope part-files in memory (sum `stack_id`
   counts, merge dicts) and return the flamegraph tree plus a `coverage` block.

It is fully stateless: folding happens only during a poll (so it stops when the
client stops polling), there is no background task or coordination, and
re-folding is safe by idempotency. The client polls until coverage freezes.

### 4. Coverage & the cap
Every demand-driven response carries:

```json
"coverage": { "files_matched": 480, "files_folded": 12, "samples_folded": 41203, "total_bytes": 1788000000, "hosts_matched": 40, "hosts_folded": 8 }
```

rendered as e.g. `12 / 480 files (2.5%) · 8 / 40 hosts · 41,203 samples` (the
host fraction shows how much of the scope's fleet breadth the sample spans, and
is dropped for single-host scopes). Folding plateaus at
the **sampling cap** = `min(max(5% × files_matched, baseline 4), 100 files)` —
the fraction keeps small scopes sensible; the absolute ceiling (100) stops a
fleet-day scope from chasing 5% of tens of thousands of files. A "Fetch more"
button raises the ceiling for a scope on demand (`max_files`).

Coverage v1 is *file coverage*. Per-node statistical confidence intervals (the
literal "how accurate is this sample") are a planned later addition.

## Storage layout

```
{output_prefix}/v{SAMPLES_FORMAT_VERSION}/bucket={source_bucket}/
  samples/service={svc}/date={YYYY-MM-DD}/host={host}/{hash}.parquet  ← one row per sample
  dict/stacks/{hash}.parquet                                          ← stack_id → frame names
  polls/{hash}.parquet                                                ← poll spans (for /tokio-stats)
```

Hive-partitioned paths make the folded-set LIST scope-prunable and give
partition pruning on the query side; the content hash is only the leaf. The
output is also namespaced by `bucket={source_bucket}` so bring-your-own-creds
sources fold into isolated, independently prunable/GC-able trees.

Two independent version knobs, deliberately in opposite places:
- **`ORDER_VERSION`** (= 1) lives *only* in the order-key hash input. Bump it to
  change the fetch-order permutation; persisted samples are order-independent
  and survive untouched.
- **`SAMPLES_FORMAT_VERSION`** (= 3) lives *only* in the output path. Bump it
  when changing *what* we persist; reads/writes then target a fresh empty tree
  that repopulates lazily on demand — no backfill job. The old tree is abandoned
  and GC'd out-of-band.

## API

### `GET /api/flamegraph`
Runs the refinement loop when the server is in demand-driven mode; otherwise
falls back to reading a pre-aggregated local dir (the old behavior).

| Param | Meaning |
|---|---|
| `service` | Exact service match (the scope's service). |
| `host` | Repeatable (`host=a&host=b`) — the scope's host set. Empty = all hosts. |
| `start_ns`, `end_ns` | Wall-clock window (epoch nanoseconds), interval-overlap matched. |
| `thread_class`, `source` | `worker`/`off-worker`, `cpu`/`sched` filters. |
| `max_files` | "Fetch more": raise the sampling-cap ceiling for this scope. |

Response: `{ tree, total_samples, coverage?, metadata }`. `coverage` is present
only in demand-driven mode.

### `GET /tokio-stats`
Same scope/refinement machinery, but reads the `polls/` part-files and returns
per-spawn-location poll statistics (durations + worst exemplars per poll class).

### `GET /api/config`
Now advertises `aggregation_enabled: bool` so the client knows whether the
flamegraph button should drive the sampled loop or the exact client-side path.

> A per-node **drill-down endpoint** (breakdown of one `stack_id` by `host`,
> `metadata.version`, …) does **not** exist yet — see "Known gaps" below.

## UI integration

The S3 browser's 🔥 **Flamegraph** button:
- **Demand-driven mode** (`aggregation_enabled`): builds a scope from the
  heatmap selection's host set + `[t0, t1]` and opens `flamegraph.html?api=1&…`,
  which polls `/api/flamegraph`, renders the coverage badge, refines in place,
  stops when coverage freezes, and offers "Fetch more".
- **Exact mode** (no aggregation): the original path — streams the selected raw
  traces and decodes them client-side. This path is preserved and still backs
  the trace-viewer pop-out (which flamegraphs a specific already-loaded trace).

## Running it

**Local (no AWS):**
```bash
dial9 serve --agg-source-dir <dir-of-raw-traces> [--agg-output-dir <dir>]
```

**S3:**
```bash
dial9 serve --bucket <raw-traces> --agg \
  [--agg-output-bucket <bucket>] [--agg-output-prefix flamegraph-data]
```

`--agg-segment-secs` (default 60) sets the segment-duration pad for the time
filter.

**Demo:** `./scripts/demo-aggregation.sh` seeds synthetic segments across hosts
and minutes, starts the server in demand-driven mode, and prints coverage
climbing from the baseline to the cap. `--serve` leaves it up for the browser.
*(Currently untracked — a working demonstration, not yet committed.)*

## Where the code lives

```
dial9-viewer/src/ingest/aggregate.rs   ← kit of parts: order key, scope→matched-set, fold_one,
                                          folded-set LIST, in-memory aggregate, coverage, versioned paths
dial9-viewer/src/ingest/refine.rs      ← refinement loop (list→scope→cap→fold→coverage), shared by all endpoints
dial9-viewer/src/ingest/decode.rs      ← raw trace bytes → resolved CPU samples + stacks + polls
dial9-viewer/src/ingest/parquet_writer.rs ← write samples / stacks / polls part-files
dial9-viewer/src/ingest/mod.rs         ← module wiring for the aggregation building blocks
dial9-viewer/src/server/flamegraph.rs  ← /api/flamegraph: refine() → aggregate samples/ → tree
dial9-viewer/src/server/tokio_stats.rs ← /tokio-stats: refine() → read polls/ → poll stats
dial9-viewer/src/server/config.rs      ← aggregation_enabled flag
dial9-viewer/src/{cli,lib}.rs          ← serve --agg / --agg-source-dir wiring
dial9-viewer/ui/{index,flamegraph}.html, flamegraph_api.js ← button + poll loop + coverage UI
dial9-viewer/tests/aggregate_test.rs   ← end-to-end flow over simulated S3 (s3s)
```

## What's tested

`tests/aggregate_test.rs` drives the real HTTP endpoint against a simulated S3
(s3s), proving:
- baseline floor folds K=4 and returns a real tree on the first refining poll;
- coverage climbs across polls and **plateaus at the cap below 100%**;
- re-folding is idempotent (no duplicate part-files; stable counts);
- "Fetch more" raises the cap;
- scope filtering by host and time selects the right files;
- a multi-host scope matches the union of those hosts' files;
- `/api/config` reports `aggregation_enabled`.

## Known gaps / deferred

- **Drill-down endpoint** — a route returning a per-dimension breakdown
  (`host`, `metadata.version`, time bucket, …) for a clicked `stack_id`. Not
  built; the tree is currently refiltered rather than broken down server-side.
- **Per-node confidence intervals** — the statistically rigorous "how accurate"
  signal. v1 ships file coverage only.
- **Per-file histogram memoization** — a poll currently re-aggregates the
  in-scope part-files; memoizing each file's immutable `{stack_id→count}` would
  turn a poll into "sum N small maps". Deferred optimization, not architecture.
- **Multi-service scopes** — the scope takes a single service; a box spanning
  several passes the first and relies on host-set/time to narrow.
- **Live S3 smoke test** — the S3 path is covered by simulated-S3 integration
  tests; a real-bucket `serve --bucket … --agg` run is unverified.
- **L0 / preload warmer** — pre-folding a minimal cache for hot scopes is out of
  scope for the core; it would just reuse the fold primitive on a schedule.

[scope]: ../CONTEXT.md
