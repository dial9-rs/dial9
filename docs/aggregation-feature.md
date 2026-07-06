# Aggregated Flamegraphs — Feature Summary

*Status: implemented (merged June 2026). This is the "what exists today and how
it works" summary. For the canonical vocabulary see [`CONTEXT.md`](../CONTEXT.md);
for the storage decision see [ADR-0003](adr/0003-folded-set-is-the-output-listing.md);
for the longer design narrative see [`aggregator.md`](../aggregator.md).*

## What it does

Builds CPU flamegraphs over a fleet's worth of dial9 traces — a time range
across many hosts — without aggregating every file first. You pick a scope
(time window + hosts), and the viewer shows a flamegraph **immediately** from a
small representative sample, then **progressively refines** it as the server
folds more files and streams updates over a single Server-Sent Events request. A
coverage label always tells you how complete the current view is.

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
| When aggregation happens | Ahead of time, whole window | At query time, streamed file-by-file |
| How much | Everything | A representative sample, capped (~5%) |
| First result | After the whole batch | The already-folded snapshot, immediately |
| Completeness signal | None | `coverage` on every SSE event |
| `samples` table | Same schema — one row per CPU sample, drilled into at query time. Now populated *incrementally and partially* instead of batch-filled. |

There is **no `dial9 ingest` batch command** — aggregation happens only inside
the `/api/flamegraph` refinement stream. Nothing pre-aggregates ahead of a query.

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

### 3. Refinement stream (one held-open SSE request)
Each `GET /api/flamegraph` request (a Server-Sent Events stream):

1. **Resolve** (once) — lists the source scope, filters to the [scope]'s
   service / host-set / time (interval-overlap on the filename `epoch`, padded
   by the segment duration), sorts by the order key, takes the first *sampling
   cap* files, and lists the output `samples/` tree (the record of what's already
   folded).
2. **Prime + emit** — reads the already-folded part-files into an in-memory
   accumulator and emits the first SSE event (the instant snapshot).
3. **Fold + stream** — folds the not-yet-folded capped files concurrently under
   the process-global `FoldLimits`; as each file lands, merges its part-file into
   the accumulator (sum `stack_id` counts, merge dicts) and emits a fresh
   flamegraph tree + `coverage` block. Closes the stream at the cap.

Folding runs only while the request is open — the fold `JoinSet` is dropped
(cancelling in-flight folds) when the client disconnects — there is no background
task or coordination, and re-folding is safe by idempotency. `coverage` climbs
monotonically until the server closes the stream at the cap.

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
A Server-Sent Events stream (`Content-Type: text/event-stream`) that runs the
refinement stream, emitting one JSON snapshot per SSE event as files fold.

| Param | Meaning |
|---|---|
| `service` | Exact service match (the scope's service). |
| `host` | Repeatable (`host=a&host=b`) — the scope's host set. Empty = all hosts. |
| `start_ns`, `end_ns` | Wall-clock window (epoch nanoseconds), interval-overlap matched. |
| `thread_class`, `source` | `worker`/`off-worker`, `cpu`/`sched` filters. |
| `max_files` | "Fetch more": raise the sampling-cap ceiling for this scope. |

Each event's `data:` payload is `{ tree, total_samples, coverage, metadata }`.
The client reads the stream via `fetch` (not `EventSource`, which can't send the
`x-dial9-aws-*` credential headers) — see `dial9-viewer/ui/sse.js`.

### `GET /api/tokio-stats`
Same scope/refinement machinery (also an SSE stream), but reads the `polls/`
part-files and returns per-spawn-location poll statistics (durations + worst
exemplars per poll class).

### `GET /api/config`
Now advertises `aggregation_enabled: bool` so the client knows whether the
flamegraph button should drive the sampled SSE stream or the exact client-side
path.

> A per-node **drill-down endpoint** (breakdown of one `stack_id` by `host`,
> `metadata.version`, …) does **not** exist yet — see "Known gaps" below.

## UI integration

The S3 browser's 🔥 **Flamegraph** button:
- **Demand-driven mode** (`aggregation_enabled`): builds a scope from the
  heatmap selection's host set + `[t0, t1]` and opens `flamegraph.html?api=1&…`,
  which opens the `/api/flamegraph` SSE stream, renders the coverage badge,
  refines in place on each event, marks the view "refined" when the server closes
  the stream at the cap, and offers "Fetch more" (reopens with a higher cap).
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
                                          folded-set LIST, incremental FlamegraphAccum, coverage, versioned paths
dial9-viewer/src/ingest/refine.rs      ← resolve (list→scope→cap→folded) + fold_stream (streamed folds), shared by all endpoints
dial9-viewer/src/ingest/decode.rs      ← raw trace bytes → resolved CPU samples + stacks + polls
dial9-viewer/src/ingest/parquet_writer.rs ← write samples / stacks / polls part-files
dial9-viewer/src/ingest/mod.rs         ← module wiring for the aggregation building blocks
dial9-viewer/src/server/flamegraph.rs  ← /api/flamegraph SSE: resolve → fold_stream → merge samples/ → tree per event
dial9-viewer/src/server/tokio_stats.rs ← /api/tokio-stats SSE: resolve → fold_stream → merge polls/ → poll stats per event
dial9-viewer/src/server/config.rs      ← aggregation_enabled flag
dial9-viewer/src/{cli,lib}.rs          ← serve --agg / --agg-source-dir wiring
dial9-viewer/ui/sse.js                 ← fetch-based SSE reader + frame decoder (EventSource can't send cred headers)
dial9-viewer/ui/{index,flamegraph,tokio_stats}.html, flamegraph_api.js ← button + SSE stream + coverage UI
dial9-viewer/tests/aggregate_test.rs   ← end-to-end SSE flow over simulated S3 (s3s)
```

## What's tested

`tests/aggregate_test.rs` drives the real HTTP SSE endpoints against a simulated
S3 (s3s), proving:
- one stream emits an already-folded snapshot first, then a real tree, folding to
  the baseline K=4;
- coverage climbs monotonically across SSE events and **stops at the cap below
  100%**;
- re-folding is idempotent (no duplicate part-files; stable counts);
- "Fetch more" raises the cap;
- scope filtering by host and time selects the right files;
- a multi-host scope matches the union of those hosts' files;
- `/api/tokio-stats` streams + refines to the cap;
- `/api/config` reports `aggregation_enabled`.

## Known gaps / deferred

- **Drill-down endpoint** — a route returning a per-dimension breakdown
  (`host`, `metadata.version`, time bucket, …) for a clicked `stack_id`. Not
  built; the tree is currently refiltered rather than broken down server-side.
- **Per-node confidence intervals** — the statistically rigorous "how accurate"
  signal. v1 ships file coverage only.
- **Cross-request histogram memoization** — within one stream the accumulator is
  incremental (each fold merges once), but a *new* stream re-reads the
  already-folded part-files to prime its accumulator. Memoizing each file's
  immutable `{stack_id→count}` across requests would make a reopened stream "sum
  N cached small maps". Deferred optimization, not architecture.
- **Multi-service scopes** — the scope takes a single service; a box spanning
  several passes the first and relies on host-set/time to narrow.
- **Live S3 smoke test** — the S3 path is covered by simulated-S3 integration
  tests; a real-bucket `serve --bucket … --agg` run is unverified.
- **L0 / preload warmer** — pre-folding a minimal cache for hot scopes is out of
  scope for the core; it would just reuse the fold primitive on a schedule.

[scope]: ../CONTEXT.md
