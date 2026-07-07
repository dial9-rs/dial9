# dial9 Aggregated Flamegraphs

Extract CPU samples from raw dial9 traces into compact Parquet "rich
flamegraph" part-files, then aggregate them into flamegraphs that can drill
down by host, source, thread class, spawn location, and time.

> **Architecture note (2026-06):** the pipeline is **demand-driven**, not
> batch. There is no "aggregate the whole window up front" step and **no
> separate manifest** of what has been processed. A query folds [source files]
> one at a time in a deterministic pseudo-random order and serves results over
> whatever subset has been folded so far, refining as it goes. Whether a file
> is already folded is answered by **listing the output part-files** — their
> existence *is* the record (see
> `docs/adr/0003-folded-set-is-the-output-listing.md`). See `CONTEXT.md` for the
> vocabulary.

---

## Mental Model

Key insight: **most of the information in 24h of profiling data is in the first
few samples.** You don't need to aggregate every minute of every hour — a small,
*representative* spread of files approximates the whole window.

1. **You pick a scope** — a time range (minute precision) and host/service
   selector. The backend lists the matching [source files] (the *matched set*).
2. **The backend orders the matched set** by `BLAKE3(ORDER_VERSION ++ source_key)`
   — deterministic, but uniform across host and time, so the first few files are
   a representative spread rather than one host's earliest minutes.
3. **One request streams the refinement** over Server-Sent Events. The server
   emits the already-folded snapshot immediately (stamped with *coverage* —
   "4 / 480 files"), then folds the not-yet-folded files up to the *sampling cap*
   (default 5% of matched, ceiling 100 files), pushing a fresh full tree as each
   file lands, and closes the stream at the cap. It deliberately never folds the
   whole window. "Fetch more" reopens the stream with a higher cap.
4. **You drill into the tree** — filter by host, source, thread class, or spawn
   location (the toolbar is driven by the *facets* the response advertises) and
   the tree recomputes over the folded set.

A *fold* decodes one file's CPU samples into a partitioned Parquet part-file
whose leaf name is `{blake3(source_key)}`. **The part-file's existence is the
record that the file is folded** — there is no manifest and no skip-set — so
folding is idempotent and even zero-sample files write an (empty) part-file so
they are never re-fetched.

[source files]: ./CONTEXT.md

---

## Data Model

The output store lives under a versioned, source-bucket-namespaced root:

```
{output_prefix}/v{SAMPLES_FORMAT_VERSION}/bucket={source_bucket}/
```

Namespacing by source bucket keeps bring-your-own-credentials sources in
isolated, independently prunable/GC-able trees, and a format-version bump points
at a fresh empty tree that repopulates lazily. Three part-file kinds live under
that root: `samples/`, `dict/stacks/`, and `polls/`.

### Samples table (`samples/`)

One row per CPU/scheduler sample. Hive-partitioned by `service/date/host`; the
leaf is the BLAKE3 of the source key:

```
samples/service={svc}/date={YYYY-MM-DD}/host={host}/{blake3(source_key)}.parquet
```

| Column | Parquet type | Nullable | Notes |
|--------|-------------|----------|-------|
| `timestamp_ns` | `INT64` | no | Wall-clock epoch ns (clock-synced from monotonic) |
| `stack_id` | `FIXED_LEN_BYTE_ARRAY(16)` | no | BLAKE3 of the frame sequence |
| `worker_id` | `UINT32` | **yes** | `null` = off-runtime (not attributed to a worker) |
| `source` | `UINT8` | no | `0` = CPU profile, `1` = scheduler / off-CPU |
| `source_key` | `STRING` | no | Origin trace segment key |
| `host` / `service` / `date` | `STRING` | no | Also stored per-row (not only path-inferred) |
| `poll_duration_ns` | `INT64` | **yes** | `null` = sample not inside a poll |
| `spawn_location` | `STRING` | **yes** | `null` = no poll / no recorded spawn location |
| `metadata` | `MAP<STRING, STRING>` | no | version, region, instance_type, … (dict-encoded) |

`service`, `date`, and `host` appear both as Hive partition components (so the
folded-set LIST is scope-prunable) **and** as stored columns (so a part-file is
self-describing when read directly). All metadata keys/values are repetitive, so
Parquet dictionary encoding makes the map nearly free.

### Stacks dictionary (`dict/stacks/`)

```
dict/stacks/{blake3(source_key)}.parquet
```

| Column | Parquet type | Notes |
|--------|-------------|-------|
| `stack_id` | `FIXED_LEN_BYTE_ARRAY(16)` | Content-addressed: `BLAKE3(frames.join("\x00"))[:16]` |
| `frames` | `LIST<STRING>` | Resolved frame names, leaf → root |

**Deduplication:** the same code path on any host produces the same `stack_id`,
so the dictionary grows sub-linearly with fleet size. Real data: 163K samples →
15K unique stacks on one host over 10 minutes (~11× dedup).

### Polls table (`polls/`)

```
polls/{blake3(source_key)}.parquet
```

One row per reconstructed poll span, used by the `/tokio-stats` endpoint.

| Column | Parquet type | Notes |
|--------|-------------|-------|
| `start_ns` / `end_ns` / `duration_ns` | `INT64` | Poll span (wall-clock ns) |
| `worker_id` | `UINT32` | Worker the poll ran on |
| `task_id` | `UINT64` | Task identity |
| `spawn_loc` | `STRING` (nullable) | Spawn location of the task |
| `cpu_sample_count` / `sched_sample_count` | `UINT32` | Samples that landed in the poll |
| `host` / `service` / `date` | `STRING` | Scope columns |

> There is **no `_manifest/` table**. "Which files are folded?" is answered by a
> scope-pruned LIST of `samples/` (see ADR-0003). Re-folding an immutable source
> file writes the same keys, so the store is idempotent with no skip-set.

---

## How a query aggregates

There is **no SQL engine** (no DataFusion): aggregation is a small in-memory
fold in `ingest/aggregate.rs::aggregate`. For each folded, in-scope `samples/` part-file
the server reads the Arrow batch, applies the active filters (`source`,
`thread_class`, `spawn_location`, and any facet selection), sums counts per
`stack_id`, and merges the matching `dict/stacks/` entries. The summed
`(stack_id → count)` pairs plus the dictionary are turned into a flamegraph
trie. Hive partition columns (`service`/`date`/`host`) prune the folded-set LIST
before any part-file is read.

Drill-down breakdowns (e.g. "count by host" or "by `metadata.version`" for a
clicked node) are **not yet a server endpoint** — see [Deferred](#deferred).

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│  Raw traces (S3 or local) — source of truth                           │
│  {prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{host}/{boot}/{ts}-{i}.bin.gz │
└────────────────────────┬───────────────────────────────────────────── ┘
                         │ folded on demand during a query poll
                         ▼
┌──────────────────────────────────────────────────────────────────────┐
│  Parquet store (S3 or local)                                          │
│  {output_prefix}/v{N}/bucket={source_bucket}/                          │
│                                                                       │
│    samples/service={svc}/date={YYYY-MM-DD}/host={host}/                │
│      {blake3(source_key)}.parquet                                      │
│    dict/stacks/{blake3(source_key)}.parquet                            │
│    polls/{blake3(source_key)}.parquet                                  │
│                                                                       │
│  (no _manifest/ — the part-file's existence is the folded record)      │
└────────────────────────┬─────────────────────────────────────────────┘
                         │ GET /api/flamegraph (SSE refinement stream)
                         ▼
┌──────────────────────────────────────────────────────────────────────┐
│  dial9 serve                                                          │
│    /api/flamegraph  → SSE: aggregated tree + coverage + facets, per file│
│    /api/tokio-stats → SSE: per-spawn-location poll stats (from polls/)  │
└──────────────────────────────────────────────────────────────────────┘
```

---

## API Endpoints

Both endpoints are **Server-Sent Event streams** (`Content-Type:
text/event-stream`). A single request holds the connection open: the server
emits the already-folded snapshot first, then folds the not-yet-folded files up
to the sampling cap and pushes a fresh full snapshot (one `data:` JSON object per
SSE event) as each file lands, closing the stream at the cap. The client
re-renders on every event — there is no client-driven polling.

Because bring-your-own-credentials rides on `x-dial9-aws-*` request headers, the
client reads the stream via `fetch` (the native `EventSource` can't set request
headers) — see `dial9-viewer/ui/sse.js`.

### `GET /api/flamegraph`

| Param | Required | Description |
|-------|----------|-------------|
| `service` | no | Service name (exact match on the `service=` path component) |
| `from` / `to` | no | Start / end hour `HH` for the scope time filter |
| `start_ns` / `end_ns` | no | Explicit wall-clock ns range (inclusive); used by region selections |
| `host` | no | Filter host(s), repeatable (`host=a&host=b`) → host set |
| `thread_class` | no | `worker` (on-runtime) / `off-worker` (off-runtime) |
| `source` | no | `cpu` / `sched` (empty = all) |
| `spawn_location` | no | Exact match on a task's spawn location |
| `max_files` | no | "Fetch more": raise the sampling-cap ceiling for this scope |
| `bucket` / `prefix` | no | Source override (bring-your-own-credentials) |

The available toolbar options are not hard-coded: each event advertises the
*facets* present in the scope (host, source, thread class, spawn location), and
the UI renders the toolbar from that array.

**Each SSE event's `data:` payload** is one JSON object:

```json
{
  "tree": { "name": "(all)", "count": 163029, "self": 0, "children": [] },
  "total_samples": 163029,
  "coverage": {
    "files_matched": 480,
    "files_folded": 24,
    "samples_folded": 163029,
    "total_bytes": 1788000000
  },
  "metadata": {
    "service": "my-service",
    "hosts": 12,
    "time_range": "2026-06-19 14:00–15:00",
    "min_timestamp_ns": 1750000000000000000,
    "max_timestamp_ns": 1750003600000000000,
    "facets": [
      { "name": "source", "label": "Source", "values": ["cpu", "sched"] },
      { "name": "host", "label": "Host", "values": ["ip-10-0-1-7", "ip-10-0-1-8"] }
    ],
    "scope": { "service": "my-service", "hosts": [], "start_ns": null, "end_ns": null, "filters": {} }
  }
}
```

`files_folded` climbs monotonically across events until it reaches the cap, at
which point the server closes the stream.

### `GET /api/tokio-stats`

Same scope/refinement machinery as `/api/flamegraph` (also an SSE stream), but
reads the `polls/` part-files and returns per-spawn-location poll statistics
(durations and worst exemplars per poll class).

---

## Refinement Loop

Aggregation is driven by the query. The engine lives in `ingest/refine.rs`,
split into two shared pieces so `/api/flamegraph` and `/api/tokio-stats` cannot
drift apart:

- **`resolve`** (once per request): list the source scope, filter to the
  [scope]'s service/host/time (interval-overlap on the filename `epoch`, padded
  by the segment duration), sort by the *order key*, take the first *cap* files
  as the capped prefix, and list the folded set (the output `samples/` tree —
  the record of what's already folded; **no manifest**).
- **`fold_stream`** (drives the SSE stream): fold the not-yet-folded capped files
  concurrently under the process-global `FoldLimits`, yielding each file as it
  lands. A *fold* is: fetch + gunzip → decode with `dial9-trace-format::Decoder`
  → resolve `callchain` to frame names → `stack_id = BLAKE3(frames)[:16]` → write
  a partitioned samples part-file (empty if zero samples), a stacks-dict
  part-file, and a polls part-file, all named `{blake3(source_key)}`.

The *endpoint* consumes the fold stream, incrementally merging each folded file's
part-files into an accumulator and emitting one SSE event per file:
`/api/flamegraph` merges `samples/` into a tree, `/api/tokio-stats` merges
`polls/` into poll stats. Both attach the same `coverage` block. It emits the
already-folded snapshot first (instant), then a refined snapshot per fold.

Stream-driven and idempotent: folding runs only while the request is open (the
fold `JoinSet` is dropped — cancelling in-flight folds — when the client
disconnects), and re-folding a file writes the same keys. The `coverage` block
(`files_matched`, `files_folded`, `samples_folded`, `total_bytes`) tells the user
how complete the view is; it climbs monotonically until the server closes the
stream at the cap.

[scope]: ./CONTEXT.md

### Demo

`./scripts/demo-aggregation.sh` seeds a local directory with synthetic segments
across several hosts/minutes, starts the viewer in demand-driven mode
(`serve --agg-source-dir …`), and polls the endpoint to show coverage climbing
from the baseline to the cap. Add `--serve` to explore it in the browser.

---

## Implementation: Crate Structure

All in `dial9-viewer` (produces the `dial9` binary):

```
dial9-viewer/src/
  ingest/
    mod.rs              — module wiring for the aggregation building blocks
    aggregate.rs        — KIT OF PARTS: order key, scope→matched set, fold_one,
                          folded-set LIST, in-memory aggregate of samples/polls
                          part-files, coverage, versioned/partitioned paths
    refine.rs           — REFINEMENT LOOP: list→scope→cap→fold→coverage,
                          shared by every demand-driven endpoint
    decode.rs           — trace bytes → (Vec<ResolvedSample>, stacks dict, Vec<ResolvedPoll>)
    parquet_writer.rs   — write the samples / stacks-dict / polls part-files
  server/
    flamegraph.rs       — /api/flamegraph: refine() → aggregate samples/ → tree
    tokio_stats.rs      — /tokio-stats: refine() → read polls/ → per-spawn stats
  storage.rs            — StorageBackend over S3 / local FS / simulated S3 (tests)
  cli.rs                — `serve [--agg | --agg-source-dir …]`, `report`, `agents`
tests/
  aggregate_test.rs     — end-to-end refinement flow over simulated S3 (s3s)
  decode_test.rs / parser_parity_test.rs — decode + JS-parity coverage
```

There is **no `ingest` CLI subcommand** and no batch orchestrator: aggregation
happens only inside the `/api/flamegraph` refinement loop.

### Dependencies

```toml
dial9-trace-format = { workspace = true, features = ["serde-deserialize"] }
dial9-core = { workspace = true }
arrow = "54"
parquet = { version = "54", features = ["arrow", "flate2", "snap"] }
blake3 = "1"
bytes = "1"
lasso = "0.7"       # string interning during decode
rustc-hash = "2"    # FxHash for hot maps
```

---

## Scale Estimates

| Dimension | Estimate |
|-----------|----------|
| Samples per host-hour | ~1.4M (99Hz × 4 workers × 3600s) |
| Unique stacks per host-hour | ~15K |
| Parquet size per host-hour | ~5-10MB |
| 100 hosts × 24h | ~12-24GB total |
| Fleet-hourly fold (sampled, not whole window) | bounded by the sampling cap |
| Dict size (fleet-wide, shared stacks) | ~50-100MB |

The sampling cap means a query never reads the whole window: it folds a
representative subset (default 5% of matched files, ceiling 100) and reports
coverage, so cost scales with the cap, not the scope size.

---

## Deferred

- **Drill-down endpoint**: a server route that returns a per-dimension breakdown
  (count by host / `metadata.version` / time bucket) for a clicked node. Today
  the tree is recomputed with filters client-side; there is no `/drill` route.
- **Compaction**: merge small part-files into larger ones. Not needed until file
  count becomes a problem.
- **Pre-aggregated trees**: write pre-built flamegraph JSON for common queries.
  Optimization, not architecture.
- **Diff endpoint**: compare two scopes, return a diff tree with per-node delta.
- **Memory profiling**: same pattern for `AllocEvent` samples.
- **Real-time freshness**: fold triggered by S3 event notifications instead of
  query-time polling.
- **Promote hot metadata keys**: if everyone filters by `version`, promote it
  from the map to a top-level dict-encoded column.
