# On-Trigger Pipeline Runs

## Overview

Issue [#469](https://github.com/dial9-rs/dial9/issues/469) asks for a mode
where dial9 keeps buffering trace segments as today (in the disk or memory
ring) but does not upload to S3 unless the application explicitly asks for
it. The motivating use case is incident debugging: applications generate
large quantities of trace data, most of which is uninteresting; the
operator only wants to pay upload cost when something noteworthy happens
(for example, a Tokio idle ratio drop, a latency spike, or an application
assertion that should never fire).

**Core principle.** The trigger controls **when** the pipeline runs, not
**what** the pipeline does. The same `SegmentProcessor` chain that
processes segments continuously today will process them on demand under
the new schedule. No new pipeline stages are required to make the trigger
work, only a different worker schedule.

This design splits the change into two layers:

- **Layer 1: generic on-trigger schedule.** A new `WorkerSchedule`
  selector and a `PipelineRun` handle let any pipeline (custom or preset)
  run on demand. No S3-specific concepts.
- **Layer 2: S3 dump sugar.** When the pipeline ends at S3 and we want
  each triggered run to be a correlatable unit (a stable id, a
  human-readable name, a dedicated key prefix, a completion sentinel),
  we add a `with_s3_dump_on_trigger` preset and a `DumpHandle`. "Dump"
  is a Layer 2 word; it implies S3 plus an addressable correlation
  unit.

The two layers are independent. A user wiring `with_custom_pipeline` plus
`with_worker_schedule(OnTrigger)` gets the timing behavior without any
dump semantics. A user wiring `with_s3_dump_on_trigger` gets both layers
in one call.

## Architecture

The writer keeps producing sealed segments into the ring exactly as today.
For the memory backend, `MemFs::seal` evicts oldest
segments on push when bytes would exceed `max_total_size`. For the disk
backend, `DiskFs::seal` renames the
active file to its sealed name and lets the file accumulate on disk under
the writer's existing budget. _Neither backend depends on the worker
running_.

In the default `Continuous` schedule, the worker loop
(`WorkerLoop::run`) pops segments from
the ring as they appear and runs each through the configured processor
chain. Between cycles it parks on `Fs::wait_for_more`.

In the new `OnTrigger` schedule, the worker parks indefinitely on a
request channel instead. It does not call `Fs::take_files` between
triggers. When a request arrives it takes a snapshot of what is sealed at
that instant and runs each captured segment through the same processor
chain. A small worker-loop hook fires once per triggered run, after the
last segment finishes its pipeline (Layer 2 uses it to write a
completion sentinel). Then the worker re-parks.

So, we end up reusing the entire `SegmentProcessor` trait, the `PipelineBuilder`
DSL, the per-segment drain code in `WorkerLoop::process_segments`,
`Fs::take_files`, panic catching, retry semantics, metrics emission. What's new is one schedule enum on the builder, one handle type returned from
`build`, one end-of-run hook.

## Layer 1: Generic on-trigger schedule

### 1. Worker schedule

A new enum exposed on the builder, orthogonal to pipeline selection:

```rust
pub enum WorkerSchedule {
    /// Today's behavior. Worker pops segments as they are sealed and
    /// runs each through the configured pipeline immediately.
    Continuous,
    /// Worker parks until a `PipelineRun::process_now` /
    /// `process_window` request arrives, then drains the snapshot
    /// through the pipeline once.
    OnTrigger,
}
```

The schedule is independent of `with_custom_pipeline`,
`with_s3_uploader`, or any future pipeline preset. A representative
composition:

```rust
let (guard, run) = TracedRuntime::builder()
    .with_custom_pipeline(|p| p.symbolize().gzip().write_back())
    .with_worker_schedule(WorkerSchedule::OnTrigger)
    .build()?;
```

The builder method attaches at the point where `TracedRuntimeBuilder`
already varies on the pipeline-state phantom type. The
schedule is carried in a separate, orthogonal field; phantom-state
transitions for pipeline choice are unaffected. Default value is
`Continuous` so existing callers see no behavior change.

### 2. Run handle

When `OnTrigger` is selected, `build` returns a `PipelineRun` handle
alongside the existing `TelemetryGuard`:

```rust
#[derive(Clone)]
pub struct PipelineRun {
    // sender into the worker's request channel
}

impl PipelineRun {
    /// Snapshot run. Captures the set of segments currently sealed at
    /// invocation, runs them through the configured pipeline, resolves
    /// after the last segment finishes its pipeline.
    pub async fn process_now(&self) -> Result<RunReceipt, RunError>;

    /// Windowed run. Captures the current snapshot as the "pre" set,
    /// then keeps consuming segments sealed during `post`. Resolves
    /// after the post window elapses and every segment finishes its
    /// pipeline.
    pub async fn process_window(&self, post: Duration)
        -> Result<RunReceipt, RunError>;
}

pub struct RunReceipt {
    pub run_id: RunId,
    pub segments_processed: usize,
    pub started_at: SystemTime,
    pub finished_at: SystemTime,
}
```

The receipt is intentionally generic. Layer 2 wraps `PipelineRun` and
returns a richer `DumpReceipt` that includes the dump's S3 prefix,
segment keys, and sentinel key.

### 3. Worker loop in OnTrigger mode

Today's run loop alternates `take_files` plus `process_segments` with
`wait_for_more`. In `OnTrigger` mode the same `WorkerLoop` selects on:

- `self.stop` (existing `CancellationToken`, used on shutdown)
- `self.fs.writer_done` (existing, used to start drain-to-empty)
- a new `request_rx: mpsc::Receiver<RunRequest>` populated by
  `PipelineRun`

The worker only calls `take_files` once a `RunRequest` is received.
Snapshot semantics fall out: whatever is sealed at the moment the
request arrives is the captured set.

Windowed semantics layer on the same path. On a windowed request the
worker captures the pre-snapshot as a `HashSet<SegmentIndex>`, arms a
`tokio::time::sleep(post)` deadline, and keeps popping segments from
`take_files` until the deadline fires. The union (pre snapshot plus
post-window arrivals) is then run through the processor chain and the
single end-of-run hook fires once.

**Memory-mode failure mode for windowed runs.** If `post` exceeds the
ring's drain budget, post-window segments may evict before the worker
claims them. This is a real loss, not a degraded-but-correct outcome.
The doc surfaces this explicitly; we do not silently mask it. The
mitigation is on the caller side: size `max_total_size` for the worst
expected `post`.

**Disk-mode behavior when parked.** Sealed files accumulate on disk
under the writer's existing budget. If the application never triggers,
the budget acts as a circular FIFO exactly as today when S3 is
unreachable (cross-reference `s3-worker-design.md` section 4 disk-space
safety). The `max_total_size` knob is the lever.

## Layer 2: S3 dump sugar

"Dump" is a Layer 2 word. It describes a triggered run whose terminal
stage is S3 and which we want to find later by id and by a
human-readable reason. Layer 2 adds four things on top of Layer 1: a
**dump id** (ULID, time-sortable), an application-supplied **name
slug**, a dedicated **key prefix** for the dump, and a **completion
sentinel**. The reason the dump fired lives in the name slug and in
the application's own logs; we deliberately do not bundle a per-dump
sidecar file because every other piece of context is already on the
per-segment S3 objects today (`background_task/s3.rs` sets `service`,
`boot-id`, `segment-index`, `start-time`, `host` as user-defined
metadata on every uploaded object). The generic primitive in Layer 1
stays neutral: `process_now`, `process_window`.

### 1. Builder preset

```rust
let (guard, dump) = TracedRuntime::builder()
    .with_s3_dump_on_trigger(s3_config)
    .build()?;
```

`with_s3_dump_on_trigger(config)` is a sibling of `with_s3_uploader`,
mutually exclusive with it (same phantom-state transition from
`PipelineUnset` to `PipelineS3`). It composes three things internally:

1. `WorkerSchedule::OnTrigger`.
2. The standard S3 pipeline (`SymbolizeProcessor` when the
   `cpu-profiling` feature is on, then `GzipCompressor`, then the S3
   uploader with the dump-aware key layout described in section 4
   below).
3. A completion-sentinel end-of-run hook described in section 5.

Users who want to add custom processors into the trigger pipeline (for
example, a custom redaction step before S3) drop down to Layer 1
manually: `with_custom_pipeline(|p| ...)` plus
`with_worker_schedule(OnTrigger)`. The S3 preset is a convenience for
the common case, not the only path.

### 2. Dump handle

```rust
#[derive(Clone)]
pub struct DumpHandle {
    // wraps PipelineRun + the S3 client and config needed to compute
    // keys and upload the sentinel
}

impl DumpHandle {
    /// Snapshot dump. `name` is a short application-supplied slug
    /// identifying why this dump fired. It is sanitized and appended
    /// to the dump id in the S3 key prefix.
    pub async fn dump_now(&self, name: impl Into<String>)
        -> Result<DumpReceipt, DumpError>;

    pub async fn dump_window(&self, post: Duration, name: impl Into<String>)
        -> Result<DumpReceipt, DumpError>;
}

pub struct DumpReceipt {
    pub dump_id: DumpId,              // ULID, time-sortable
    pub name: String,                 // sanitized echo of the input
    pub prefix: String,               // dumps/{dump_id}-{name}/
    pub segment_keys: Vec<String>,    // full S3 keys for the trace files
    pub sentinel_key: String,         // dumps/{dump_id}-{name}/_complete
    pub time_range: (SystemTime, SystemTime),
}
```

Name sanitization rules: ASCII alphanumerics, hyphen, underscore,
period. Anything else is replaced with `-`. Empty name is allowed; the
path becomes `dumps/{dump_id}/...` (no trailing hyphen). Max length 64
characters; longer inputs are truncated. The receipt echoes back the
sanitized form so the application can log exactly what was used.

Internally `DumpHandle` mints a `DumpId` (ULID for sortability) and
sanitizes `name`, then threads both through the existing
`SegmentData::metadata` HashMap so the S3 uploader can use them when
computing the object key (section 4). After Layer 1 finishes draining
the captured set, the end-of-run hook uses the same id and name to
write the sentinel (section 5).

### 3. What gets uploaded per dump

Per dump we push two kinds of artifacts to S3:

**Segment files.** Each captured segment is the same binary trace
format produced today, gzipped. A `.bin.gz` is self-contained and
decoder readable: it carries the schema preamble plus the event stream
(CPU samples, task spans, park/unpark, tracing-layer events, process
resource usage). No new file format.

Every segment uploaded in a dump carries the same per-object S3
user-defined metadata as continuous-mode uploads
(`service`, `boot-id`, `segment-index`, `start-time`, `host`; see
`background_task/s3.rs`) plus two new keys:

```rust
.metadata("dump-id", dump_id.to_string())
.metadata("dump-name", &sanitized_name)
```

These ride on every segment object in the dump. They are redundant
with the key path (which also contains both) but cheap, and they make
a single object self-describing without parsing the key.

**Completion sentinel.** A zero-byte object at `_complete` in the
dump's prefix. Written **last**, after every captured segment has
finished uploading. Its presence is the atomic "this dump is done"
signal for any reader. A reader observing a `dumps/{dump_id}-{name}/`
prefix without `_complete` treats the dump as in-flight or failed.

That is the complete artifact list per dump: N segment objects plus
one sentinel. No sidecar file, no index. Section 4 below lays out
where each piece of per-dump context lives so a reader needs nothing
beyond the segments themselves.

### 4. S3 key layout for dumps

The dump layout differs deliberately from the continuous-mode layout.
Today (`background_task/s3.rs` `object_key`, around line 150) the key
is time-first:

```
{prefix}/{date-time}/{service}/{instance}/{epoch_secs}-{index}.bin.gz
```

The rationale for that layout (incident correlation across services,
Athena partitioning, lifecycle policies) is documented in
`s3-worker-design.md` section 2. For dumps the access pattern is
different: a dump is its own correlation unit, addressed by its id, not
by a minute bucket.

Dump key layout:

```
{prefix}/dumps/{dump_id}-{name}/{epoch_secs}-{index}.bin.gz
{prefix}/dumps/{dump_id}-{name}/_complete
```

When `name` is empty after sanitization, the trailing hyphen is
dropped:

```
{prefix}/dumps/{dump_id}/{epoch_secs}-{index}.bin.gz
{prefix}/dumps/{dump_id}/_complete
```

Listing all dumps is `ListObjectsV2 prefix={prefix}/dumps/` and returns
ULID-prefixed entries sorted by time (ULIDs encode their timestamp in
their leading bits). Listing a single dump is `ListObjectsV2
prefix={prefix}/dumps/{dump_id}-`.

Where each piece of per-dump context lives:

| Field                        | Where it lives                                                                      |
| ---------------------------- | ----------------------------------------------------------------------------------- |
| `dump_id`                    | Key path; also `dump-id` S3 user metadata on every segment                          |
| `name` (the "why")           | Key path; also `dump-name` S3 user metadata on every segment                        |
| `triggered_at`               | Embedded in the ULID (`DumpId` can extract); also the `LastModified` of `_complete` |
| `service`, `boot_id`, `host` | Existing per-segment S3 user metadata, unchanged                                    |
| `segment.index`              | Key path; also `segment-index` S3 user metadata                                     |
| `segment.size_bytes`         | S3 object `ContentLength` (free)                                                    |
| `segment.start_epoch`        | Key path; also `start-time` S3 user metadata                                        |
| `total_compressed_bytes`     | Sum of `ContentLength` from `ListObjectsV2`                                         |
| Completion                   | `_complete` sentinel                                                                |

Implementation: the S3 processor selects the dump path when the
processor metadata map contains a `dump_id` key, otherwise it falls
through to today's continuous-mode layout. `DumpHandle` is the only
thing that inserts `dump_id`. `with_s3_uploader` and the continuous
layout are untouched.

### 5. End-of-run hook: upload the sentinel

The sentinel cannot live in a per-segment `SegmentProcessor`. No
individual processor knows that it just processed the last segment of
a run; the iteration over captured segments lives in
`WorkerLoop::process_segments`, and the S3 uploader only ever sees one
segment at a time.

We add a small worker-loop hook, called once per triggered run after
the last segment's pipeline finishes. The S3 dump preset registers a
hook implementation that:

1. Collects the per-segment S3 keys produced during the run (the S3
   uploader returns each on success; the hook accumulates them in a
   side channel for the receipt).
2. Issues a single `PutObject` with zero-byte body at
   `{prefix}/dumps/{dump_id}-{name}/_complete`, using the same
   `aws_sdk_s3::Client` the uploader used.
3. Resolves the `DumpReceipt` future returned to the caller, with the
   collected segment keys, the sentinel key, and the observed time
   range.

On sentinel upload failure, the `DumpReceipt` future resolves with
`DumpError`. The segments themselves may all be present in S3 in that
case, but without `_complete` the dump is effectively in-flight from a
reader's perspective. The caller can retry the sentinel upload via a
follow-up call (which would mint a new dump id, since v1 does not
support resuming an existing dump).

The hook surface stays internal in v1. If Layer 1 users later want an
end-of-run extension point of their own, we expose it then. We do not
promise it now.

## Snapshot vs windowed: side-by-side

| Aspect                   | `dump_now` (snapshot)                   | `dump_window(post)` (windowed)                              |
| ------------------------ | --------------------------------------- | ----------------------------------------------------------- |
| Pre-trigger context      | Whole ring at trigger time              | Whole ring at trigger time                                  |
| Post-trigger context     | None                                    | `post` seconds                                              |
| Future resolves          | After upload of captured set + sentinel | After `post` + upload + sentinel                            |
| Memory-mode failure mode | None new                                | Post segments may evict if `post` exceeds ring drain budget |
| API shape                | One method per layer                    | One method per layer                                        |
| Worker complexity        | One wake source                         | Wake source + deadline + pre-snapshot set                   |
| Implementation effort    | Small                                   | Medium                                                      |

Two small clarifications on top of the table:

- `dump_now` is semantically equivalent to `dump_window(Duration::ZERO)`.
  We keep both because the common case deserves the simpler signature
  and a future that resolves as soon as the snapshot uploads, without
  the deadline-arming code path.
- Overlapping windowed runs are not supported in v1. Calling
  `dump_window` while a window is in flight returns
  `DumpError::DumpInFlight`. The same constraint at Layer 1:
  `process_window` while a window is in flight returns
  `RunError::RunInFlight`. Revisit if a user asks; for now the
  single-window restriction has no real downside for the motivating
  use case.

## What is deliberately out of scope

- **Default triggers.** Tokio idle ratio, latency thresholds, error
  rates, and similar policy live in the application, not the library.
  We ship the primitive; users wire the policy. Example recipes belong
  under `examples/`, not in the public API.
- **Per-dump sidecar file.** All correlation data lives in the key
  path and on per-object S3 user metadata (section 4 maps each field).
  A tiny sidecar carrying only `{dump_id, name, schema_version}` would
  add no information beyond what is already in the prefix and on the
  objects, and would commit us to a versioned JSON surface we would
  have to maintain.
- **Structured k/v labels** on a dump. Only a single `name` slug is
  supported. If a user later needs structured labels, two options
  exist (S3 object tagging, a thin sidecar JSON) and neither blocks
  v1.
- **Per-segment marking.** The issue thread surfaces a "mark eligible
  for upload" framing (zz85's coworker). It is more expressive, but it
  requires plumbing a marker through `SegmentRef`, `MemorySegment`, and
  the writer-side seal path. No clear demand. Revisit if a real user
  asks.
- **Time-range dumps.** A `dump_range(start_epoch, end_epoch)` would
  filter the ring by segment header epoch before draining. The ring is
  short relative to the windows users typically care about, so "the
  ring" usually overlaps the range of interest already. Adds API
  surface for small expected payoff.
- **Non-S3 dump semantics.** Layer 2 ships S3 only. The Layer 1
  primitive is general by construction, so write-back-on-trigger for
  local debugging falls out naturally for users who compose it
  themselves. We do not invent "dump-to-disk" or "dump-to-custom"
  presets.
- **Coalescing under burst triggers.** Application concern. If a
  trigger fires a hundred times in a second the application should
  debounce; the library will not.
- **Speculative work between triggers** (for example, pre-symbolizing
  newly sealed segments while the worker is parked). Defeats the
  purpose of `OnTrigger`. Not added.

## Open questions

- **Single `process_window` instead of `process_now` + `process_window`?**
  Collapse would reduce the API to one method per layer. Recommendation:
  keep both. The no-wait case is the common case and a simpler signature
  is worthwhile; the future also resolves faster because it never arms a
  deadline.
- **Symbolization cost on trigger.** The standard S3 pipeline prepends
  `SymbolizeProcessor` when the `cpu-profiling` feature is on; that
  stage is CPU-heavy. A dump triggered during a hot moment will spike
  the worker thread. The worker is on a dedicated tokio current-thread
  runtime so it cannot steal time from the application's runtime, but
  the spike is real and worth flagging in user-facing docs. No design
  change implied.
- **Name sanitization rules.** Proposal: ASCII alphanumerics plus
  `-_.`, max 64 characters, invalid characters replaced with `-`.
  Open to a stricter rule if it turns out S3 lifecycle policies or
  consumer tooling impose narrower constraints (for example, prefixes
  that are also Glue catalog partition keys).
- **Disk-mode unbounded growth when never triggered.** If the
  application installs `OnTrigger` and never triggers, the writer keeps
  sealing files into the disk ring. The existing `max_total_size`
  budget bounds this, but the user must set it consciously (the typical
  default is generous because the worker normally consumes files
  quickly). Document; the lever already exists.
