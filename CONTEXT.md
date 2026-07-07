# Glossary

Project-wide vocabulary. Terms here are the canonical names used in code,
docs, issues, and PRs. If a term you need isn't here, either it's not yet
resolved (raise it) or you're inventing language the project doesn't use
(reconsider).

## Aggregation pipeline

**Source file**:
One immutable raw trace segment in S3
(`…/{date}/{HHMM}/{service}/{host}/{boot}/{ts}-{i}.bin.gz`). The atomic
unit of incremental aggregation work: ordered, fetched, decoded, and
folded into the `samples` table one at a time. Identified by its
`source_key`.
_Avoid_: segment (ambiguous with trace-format segments), object.

**Samples table**:
The facts table. One row per raw CPU sample (`timestamp_ns`, `stack_id`,
`thread_class`, `source`, `host`, …), stored as partitioned Parquet.
Aggregated (`GROUP BY stack_id`) at query time, never pre-folded — it
retains per-sample detail so the viewer can drill into any dimension. It
is populated *incrementally and partially* by demand-driven aggregation,
not batch-filled up front.
_Avoid_: facts table (use "samples table"), histogram store.

**Order key**:
`BLAKE3(ORDER_VERSION ++ source_key)`, ascending. A deterministic
pseudo-random total order over the [[source-file]] set that is uniform
across host and time, so the first K files are a representative spread of
the scope rather than one host's earliest minutes. Recomputed at query
time; produces no persisted artifact. "First K" is relative to the set a
query matches, so it shifts as new files land.
_Avoid_: shuffle, sort order.

**ORDER_VERSION**:
A baked constant in the hash input of the [[order-key]]. Bump it to change
the *scheduling* permutation. Lives **only** in the hash input — never in
an output path — because the samples cache is order-independent and must
survive a version bump untouched.

**SAMPLES_FORMAT_VERSION**:
A baked constant in the **output key path**
(`{output_prefix}/v{N}/samples/…`). Bump it when changing *what we
persist* and we want a deliberate recompute. The bump points reads/writes
at a fresh empty tree; files repopulate lazily on demand (no backfill
job). The old tree is abandoned and GC'd out-of-band.

**Refinement loop**:
The query control flow, realized as a **single held-open SSE stream** per
request. The server *resolves* the scope once (list → [[order-key]] sort →
take the first [[sampling-cap]] files → list the folded set), emits the
already-folded snapshot immediately, then folds the not-yet-folded
[[source-file]]s (in order) concurrently and pushes a fresh tree +
[[coverage]] block as each file lands, closing the stream at the cap. The
endpoint merges each folded file into an in-memory accumulator incrementally
(one merge per file), rather than re-scanning the whole folded set per event.
No background tasks and no coordination — folding runs only while the request
is open (the fold `JoinSet` is dropped, cancelling in-flight folds, when the
client disconnects), and re-folding is safe by idempotency. Coverage climbs
monotonically until the server closes the stream at the cap.
(Deferred optimization: memoize each file's immutable `{stack_id→count}`
histogram *across requests* so a reopened stream sums cached per-file maps.)
_Avoid_: client re-polling, `refine=` request flag, job handle, background
daemon (all superseded by the stream).

**Coverage**:
How much of a query's matched scope has been folded so far, reported on
every [[refinement-loop]] response so users know how complete the view is.
v1 = file coverage: `{ files_matched, files_folded, samples_folded }`,
shown as e.g. "12 / 480 files (2.5%)". The literal statistical accuracy
(per-node confidence intervals that tighten as files fold) is a planned
later addition, not v1.
_Avoid_: progress, accuracy (reserve "accuracy" for the future per-node CIs).

**Fold**:
Decode one [[source-file]] and write its CPU samples as a deterministically
named part-file
(`samples/service=…/date=…/host=…/{blake3(source_key)}.parquet`). A
zero-sample file still writes an empty part-file. The part-file's
*existence* is the record that the file is folded — there is no manifest
and no skip-set (see ADR-0003). Re-folding writes the same key, so it is
idempotent.
_Avoid_: ingest (reserve for the optional out-of-scope warmer), process.

**Matched set / Folded set**:
**Matched set** = the [[source-file]]s a query's scope selects (a LIST of
the source prefixes); the [[coverage]] denominator and the [[order-key]]
input. **Folded set** = those already written to `samples` (a
scope-pruned LIST of the partitioned `samples/` tree). Coverage is their
intersection.

**Baseline floor**:
The minimum number of [[source-file]]s (in [[order-key]] order) a query
folds — the floor on the [[sampling-cap]], so even a tiny scope folds a
non-trivial sample. Default **4**, configurable. Small because each file is
~37–50 MB; the [[coverage]] label, not a large floor, is what keeps users
from over-trusting an early tree. The [[refinement-loop]] streams the rest.
Moot if a preload warmer is enabled.
_Avoid_: baseline sample count (it's files, not samples).

**Sampling cap**:
The point at which the [[refinement-loop]] stops folding a scope's tail:
`min(percentage × files_matched, absolute_ceiling)`, floored at the
[[baseline-floor]]. Defaults **5%** and **100 files**, both
backend-configurable. The percentage keeps small scopes sensible; the
absolute ceiling stops a fleet-day scope from chasing 5% of tens of
thousands of files (which would re-create the batch job). The
[[refinement-loop]] closes the stream once it reaches the cap. A user-facing
"fetch more" reopens the stream with a raised ceiling for that scope on
demand. Folding also stops early if the client disconnects
(refine-while-watched).
_Avoid_: completion target (we deliberately never reach 100%).

**Scope**:
A query's selection: a time range (minute precision), a service, and a
**host set** (the heatmap box can span many hosts → `host=` is repeatable;
empty = all hosts, each entry an exact match). Translated to the
[[matched-set]] in two stages: (1) a coarse S3 prefix prune over the
`{date}/{HHMM}/` rotation buckets the window spans, widened by one bucket
each side; (2) an in-memory interval-overlap filter on each file's filename
`epoch` (padded by the known raw-trace segment duration), plus the
service/host-set filter. Time/`HHMM` is high in the key so it prunes by
prefix; service/host is below it so it is an in-memory filter. The S3
browser's 🔥 button builds a scope from the heatmap selection's hosts +
`[t0,t1]` and drives the [[refinement-loop]] when the server advertises
`aggregation_enabled` via `/api/config`.
_Avoid_: query (use "scope" for the selection, "query" for the request).

## Worker attribution

A **worker** is a tokio runtime thread with a stable `WorkerId`. A worker
runs on an OS thread, identified by its `tid`.

The `tid → worker_id` mapping is **not constant**. During
`tokio::task::block_in_place`, a worker's OS thread temporarily acts as a
blocking-pool thread (and tokio races a blocking-pool thread to take
over the worker's scheduler responsibilities on a fresh OS thread). The
mapping must therefore be reconstructed per time interval, not assumed
fixed for the trace. The same tid can also leave and later re-acquire a
worker role: it joins the blocking pool when displaced, and any
blocking-pool thread (including the original) may race to take over a
worker's core after a future `block_in_place`.

- **Authoritative source:** `WorkerPark.tid` / `WorkerUnpark.tid`. These
  bracket the intervals during which a given `tid` is acting as a given
  `worker_id`.
- **Unreliable hint:** `CpuSample.worker_id` on the wire. The producer
  fills it from a `tid → worker_id` table updated only on
  `on_thread_start` / `on_thread_stop`, which `block_in_place` does not
  fire. Analysis tooling MUST ignore this field and re-derive worker
  attribution from park/unpark events. The field will be removed from the
  trace format in a future change.

## Block-in-place gap

A **block-in-place gap** is the interval during which a worker `W`'s
binding to an OS thread is unknowable from the trace alone. It is
detected post-hoc from a `WorkerPark { worker_id: W, tid: B }` for which
the most recent prior park/unpark on worker `W` was on a different tid
`A`.

The gap is the interval `[last_event_on_W_at_A, park_at_B)`. Within the
gap, the analysis layer treats samples on tids `A` and `B` as
**unattributable to a worker** — the handoff happened at an unknown
point in the interval, so neither tid can be confidently labeled.
Samples are still visible by tid, but worker-derived views (e.g.,
flamegraphs grouped by worker) must drop them.

The viewer surfaces the gap to the user as a `block_in_place` annotation
on worker `W`'s timeline. Detailed investigation (which tid was running
which code at which moment) is left to the agent analysis toolkit, which
can correlate by tid against CPU samples, sched events, and stack
contents.

When a gap is detected on worker `W`, any **active span** (the
unpark→park interval from which CPU-time ratios are computed) that
crosses the gap is **discarded**, not split. The CPU-time delta across
the gap mixes two different threads' `CLOCK_THREAD_CPUTIME_ID` readings
and is therefore meaningless; dropping the polluted span is more honest
than reporting a contaminated ratio.
