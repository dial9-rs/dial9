# dial9 Aggregated Flamegraphs

Extract CPU samples from raw dial9 traces into compact Parquet "rich
flamegraph" files. Query them with DataFusion to build flamegraphs that can
drill down by host, version, time, and arbitrary metadata.

> **Architecture note (2026-06):** the pipeline is now **demand-driven**, not
> batch. There is no "aggregate the whole window up front" step. A query folds
> [source files] one at a time in a deterministic pseudo-random order and serves
> results over whatever subset has been folded so far, refining as it goes. See
> `CONTEXT.md` for the vocabulary and `docs/adr/0003-folded-set-is-the-output-listing.md`
> for the storage decision. The sections below describe the data model and
> query patterns, which are unchanged; the "Ingest Pipeline" section is
> superseded by the refinement loop.

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
3. **The first poll folds a baseline (3 files)** and returns a flamegraph
   immediately, stamped with *coverage* ("3 / 480 files"). The client re-polls;
   each poll folds a few more files and the tree refines, until coverage
   plateaus at the *sampling cap* (default 10% of matched, ceiling 300 files).
   It deliberately never folds the whole window.
4. **You drill into nodes** — click a stack frame, see breakdown by host,
   version, time bucket. "90% from one host" → go investigate that host. "Only
   in v2.3.0" → it's a regression.

A *fold* decodes one file's CPU samples into a partitioned Parquet part-file
whose name is `{blake3(source_key)}`. The part-file's existence *is* the record
that the file is folded (no manifest, no skip-set), so folding is idempotent and
zero-sample files are recorded too.

[source files]: ./CONTEXT.md

---

## Data Model

### Samples table (`samples/`)

One row per CPU sample. Partitioned by `service/date/hour/host` (Hive-style paths).

| Column | Parquet type | Encoding | Notes |
|--------|-------------|----------|-------|
| `timestamp_ns` | `INT64` | delta | Back-correlate to source trace |
| `stack_id` | `FIXED_LEN_BYTE_ARRAY(16)` | **dictionary** | BLAKE3 hash of frame sequence |
| `thread_class` | `INT32` (uint8) | RLE | 0=off-worker, 1=worker |
| `source` | `INT32` (uint8) | RLE | 0=cpu, 1=sched-off |
| `source_key` | `BYTE_ARRAY` (string) | dictionary | S3 key of origin trace segment |
| `metadata` | `MAP<STRING, STRING>` | dict keys + dict values | version, region, instance_type, canary_id, ... |

Partition columns (`service`, `date`, `hour`, `host`) inferred from path, not stored per-row. DataFusion exposes them as virtual columns for GROUP BY / filter.

**`metadata` map:** Populated from:
- S3 key structure (service, host, boot_id)
- Trace segment metadata headers (version, region, instance_type — whatever the app sets)
- Anything else attached at ingest time

All keys and values are heavily repetitive → Parquet's dictionary encoding on the
inner key/value arrays makes this nearly free in terms of space.

### Stacks dictionary (`dict/stacks/`)

| Column | Parquet type | Notes |
|--------|-------------|-------|
| `stack_id` | `FIXED_LEN_BYTE_ARRAY(16)` | Content-addressed: `BLAKE3(frames.join("\x00"))[:16]` |
| `frames` | `LIST<BYTE_ARRAY>` (list of strings) | Resolved frame names, leaf → root |

**Deduplication:** Same code paths across hosts produce identical `stack_id`. The
dictionary grows sub-linearly with fleet size. Real data: 163K samples → 15K unique
stacks on one host over 10 minutes (11× dedup ratio).

### Manifest (`_manifest/`)

| Column | Parquet type | Notes |
|--------|-------------|-------|
| `source_key` | `STRING` | S3 key of ingested trace segment |
| `source_etag` | `STRING` | Change detection |
| `run_id` | `STRING` | ULID of ingest run |
| `ingested_at` | `INT64` | Epoch ms |

Append-only. Ingest reads this to skip already-processed segments.

---

## Query Patterns

### Basic flamegraph

```sql
SELECT stack_id, COUNT(*) as count
FROM samples
WHERE service = 'my-svc' AND date = '2026-06-19' AND hour BETWEEN 14 AND 15
GROUP BY stack_id
```

→ Join with `dict/stacks` → build trie → return JSON.

### Drill into a node (breakdown by host)

```sql
SELECT host, COUNT(*) as count
FROM samples
WHERE stack_id = X   -- or stack_id IN (all descendants of clicked node)
  AND service = 'my-svc' AND date = '2026-06-19'
GROUP BY host
ORDER BY count DESC
```

### Drill into a node (breakdown by version)

```sql
SELECT metadata['version'] as version, COUNT(*) as count
FROM samples
WHERE stack_id = X
GROUP BY metadata['version']
ORDER BY count DESC
```

### Time breakdown (when did this spike?)

```sql
SELECT hour, COUNT(*) as count
FROM samples
WHERE stack_id = X AND service = 'my-svc' AND date = '2026-06-19'
GROUP BY hour
```

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│  Raw traces (S3) — source of truth                                   │
│  {prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{host}/{boot}/{ts}-{i}.bin.gz│
└────────────────────────┬────────────────────────────────────────────┘
                         │ dial9 ingest (background)
                         ▼
┌─────────────────────────────────────────────────────────────────────┐
│  Parquet store (S3)                                                  │
│                                                                      │
│  samples/service={svc}/date={YYYY-MM-DD}/hour={HH}/host={host}/      │
│    part-{run_id}.parquet                                             │
│                                                                      │
│  dict/stacks/part-{run_id}.parquet                                   │
│                                                                      │
│  _manifest/part-{run_id}.parquet                                     │
└────────────────────────┬────────────────────────────────────────────┘
                         │ GET /api/flamegraph
                         ▼
┌─────────────────────────────────────────────────────────────────────┐
│  dial9 serve (existing server + new endpoints)                       │
│                                                                      │
│  /api/flamegraph    → aggregated tree                                │
│  /api/flamegraph/drill → breakdown by dimension                      │
└─────────────────────────────────────────────────────────────────────┘
```

---

## API Endpoints

### `GET /api/flamegraph`

| Param | Required | Description |
|-------|----------|-------------|
| `service` | yes | Service name |
| `date` | no | `YYYY-MM-DD` (default: today) |
| `from` | no | Start hour `HH` (default: 00) |
| `to` | no | End hour `HH` (default: 23) |
| `host` | no | Filter host(s), repeatable |
| `thread_class` | no | `worker` / `off-worker` |
| `source` | no | `cpu` / `sched` |
| `metadata.*` | no | Filter by metadata key, e.g. `metadata.version=2.3.1` |

**Response:**

```json
{
  "tree": {
    "name": "(all)",
    "count": 163029,
    "self": 0,
    "children": [...]
  },
  "total_samples": 163029,
  "metadata": {
    "service": "my-service",
    "hosts": 12,
    "time_range": "2026-06-19 14:00–15:00",
    "segments_ingested": 264
  }
}
```

### `GET /api/flamegraph/drill`

Same filter params as above, plus:

| Param | Required | Description |
|-------|----------|-------------|
| `stack_id` | yes | Hex-encoded stack_id to drill into |
| `group_by` | yes | Dimension: `host`, `metadata.version`, `hour`, etc. |

**Response:**

```json
{
  "stack": "my_app::handle_request → hyper::proto::h1::dispatch::...",
  "total": 5000,
  "breakdown": [
    { "key": "ip-10-0-1-7", "count": 4500 },
    { "key": "ip-10-0-1-8", "count": 500 }
  ]
}
```

---

## Refinement Loop (replaces batch ingest)

Aggregation is driven by the query, in `server/flamegraph.rs::run_refinement_loop`.
Each `GET /api/flamegraph` poll:

1. **List the matched set** — list the source scope, filter to the [scope]'s
   service/host/time (interval-overlap on the filename `epoch`, padded by the
   segment duration), and sort by the *order key*.
2. **List the folded set** — list the output `samples/` tree (the record of
   what's already folded; no manifest).
3. **Fold a bounded budget** of not-yet-folded files in order (baseline = 3 on
   the first poll, a refine-batch of 8 after), stopping at the sampling cap.
   A *fold* is: fetch + gunzip → decode with `dial9-trace-format::Decoder` →
   resolve `callchain` to frame names → `stack_id = BLAKE3(frames)[:16]` →
   write a partitioned samples part-file (empty if zero samples) + a stacks-dict
   part-file, both named `{blake3(source_key)}`.
4. **Aggregate** the folded-in-scope part-files in memory (sum `stack_id`
   counts, merge dicts) → flamegraph tree + a `coverage` block.

Stateless and idempotent: folding happens only during a poll (so it stops when
the client stops polling), and re-folding a file writes the same keys. The
`coverage` block (`files_matched`, `files_folded`, `samples_folded`) tells the
user how complete the view is; the client polls until it freezes.

The old `dial9 ingest` batch command still exists as an optional cache-warmer
(it pre-folds files using the same primitives) but is no longer the path the
viewer depends on.

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
    aggregate.rs        — DEMAND-DRIVEN CORE: order key, scope→matched set,
                          fold_one, in-memory aggregate, coverage, versioned paths
    mod.rs              — legacy batch ingest (optional cache-warmer)
    decode.rs           — trace bytes → (Vec<ResolvedSample>, stacks dict)
    parquet_writer.rs   — write samples + dict Parquet part-files
  server/
    flamegraph.rs       — /api/flamegraph (refinement loop) + /drill
  cli.rs                — `serve --agg-source-dir …`, `ingest` subcommand
tests/
  aggregate_test.rs     — end-to-end refinement flow over simulated S3 (s3s)
```

### Dependencies

```toml
dial9-trace-format = { workspace = true }
arrow = "57"
parquet = "57"
datafusion = "54"
blake3 = "1"
ulid = "1"
rayon = "1"
```

---

## Vertical Slices

### Slice 1: Decode pipeline (AFK)

Add `dial9-trace-format` dep. Write `ingest/decode.rs`: given raw trace bytes,
extract `Vec<SampleRow>` (timestamp, resolved stack frames, thread_class, source)
and `Vec<StackEntry>` (stack_id, frames). Unit test against existing test traces.

### Slice 2: Parquet writer (AFK, blocked by #1)

Write `ingest/parquet.rs`: given samples + stacks, write partitioned Parquet files
matching the schema above. Test round-trip: write then read back with arrow.

### Slice 3: Ingest orchestrator (AFK, blocked by #1 + #2)

Write `ingest/mod.rs` + CLI subcommand. Lists source segments, skips
already-ingested (manifest), fetches in parallel, decodes, writes Parquet.
Integration test with LocalBackend + real test traces.

### Slice 4: `/api/flamegraph` endpoint (AFK, blocked by #2)

DataFusion reads samples Parquet, GROUP BY stack_id, joins dict, builds trie,
returns JSON. Test with pre-written Parquet fixtures.

### Slice 5: `/api/flamegraph/drill` endpoint (AFK, blocked by #4)

GROUP BY the requested dimension for a given stack_id. Returns breakdown.

### Slice 6: Client integration (AFK, blocked by #4)

Extend `flamegraph.html` to accept `?api=1` mode — fetch JSON tree from
`/api/flamegraph`, render with existing `FlamegraphRenderer`.

### Slice 7: Drill-down UI (AFK, blocked by #5 + #6)

Click a flamegraph node → call `/api/flamegraph/drill` → show breakdown panel.

---

## Scale Estimates

| Dimension | Estimate |
|-----------|----------|
| Samples per host-hour | ~1.4M (99Hz × 4 workers × 3600s) |
| Unique stacks per host-hour | ~15K |
| Parquet size per host-hour | ~5-10MB |
| 100 hosts × 24h | ~12-24GB total |
| Fleet-hourly query (100 hosts, 1 hour) | ~500MB-1GB Parquet read |
| Dict size (fleet-wide, shared stacks) | ~50-100MB |

---

## Deferred

- **Compaction**: Merge small part-files into larger ones. Not needed until file count becomes a problem.
- **Pre-aggregated trees**: Write pre-built flamegraph JSON for common queries (fleet-daily). Optimization, not architecture.
- **Diff endpoint**: Compare two scopes, return a diff tree with per-node delta.
- **Memory profiling**: Same pattern for `AllocEvent` samples.
- **Real-time freshness**: Ingest triggered by S3 event notifications instead of polling.
- **Promote hot metadata keys**: If everyone filters by `version`, promote it from the map to a top-level dict-encoded column.
