# On-Trigger Pipeline Runs

## Overview

Issue [#469](https://github.com/dial9-rs/dial9/issues/469) asks for a mode
where dial9 keeps buffering trace segments as today (in the disk or memory
ring) but does not upload to S3 unless the application explicitly asks for
it. Applications generate large quantities of trace data, most of which is
uninteresting; the operator only wants to pay upload cost when something
noteworthy happens (a Tokio idle ratio drop, a latency spike, an
application assertion that should never fire).

The trigger controls **when** the pipeline runs, not **what** it does. The
same `SegmentProcessor` chain that processes segments continuously today
runs on demand under the new schedule. The wire-up is one new builder
method (`with_trigger`), one cloneable control type, and a metadata
convention the S3 uploader already knows how to read.

## Usage examples

`with_trigger(rx)` is orthogonal to pipeline selection. Whichever
pipeline shape you would have wired for continuous mode, you keep, and
adding `with_trigger(rx)` flips that same pipeline into on-demand
operation. Three representative compositions follow.

### S3 preset

The default for most callers: `with_s3_uploader(config)` builds the
standard `[Symbolize?, Gzip, S3]` pipeline and auto-populates writer
segment metadata.

```rust
use dial9::trigger;

let (control, rx) = trigger::channel();

let _guard = TracedRuntime::builder()
    .with_s3_uploader(s3_config.clone())
    .with_trigger(rx)
    .build()?;

// Hand `control` to whatever subsystem decides when to dump
// (an idle-ratio watcher, a panic hook, a `/dump` HTTP handler, ...).
// `TriggerControl` is `Clone`; share it freely.

let receipt = control
    .dump_window(Some(Duration::from_secs(300)), Duration::from_secs(60))
    .with_metadata([("reason".into(), "idle-ratio-drop".into())].into())
    .await?;

tracing::info!(
    dump_id = %receipt.dump_id,
    segments = receipt.segments_processed,
    "dump complete",
);
```

That is the whole user-facing surface for the S3 case. No
`WorkerSchedule` enum, no dump-specific builder preset, no end-of-run
callback to plumb. The application registers a receiver, holds a
sender, and awaits a receipt. Completion is signalled by the receipt
resolving; applications that need a durable cross-process signal can
publish it through whatever channel they already use (incidents-table
row, Slack message, etc.).

### Custom pipeline

`with_custom_pipeline(|p| ...)` gives you the full chain. The trigger
feature is pipeline-agnostic; it stamps `dump_id` onto segment
metadata before any stage runs, and stages decide what to do with it.
Two interesting variants.

**Custom pipeline ending at S3.** Equivalent to the preset but you
control the exact stage list (omit symbolize, add a redactor, etc.).
The `s3()` stage reads `metadata.get("dump_id")` the same way the
preset does, so the dump key layout applies automatically.

```rust
let _guard = TracedRuntime::builder()
    .with_custom_pipeline(|p| p.symbolize().redact(my_redactor).gzip().s3(s3_config.clone()))
    .with_trigger(rx)
    .build()?;

// `control.dump(...)` is identical to the S3-preset example. The
// dump-style S3 key layout is produced by the `.s3(...)` stage in the
// chain, not by the preset.
```

Unlike the preset, the custom path does not auto-populate writer
segment metadata. If you want identity entries (service, host, etc.)
embedded in trace files, call `with_segment_metadata(...)` explicitly.

**Custom pipeline ending at `write_back()` (no S3).** Dumps to disk
under the writer's directory. There is no S3 stage, so no
`dumps/{dump_id}/` prefix. The receipt still carries `dump_id`, and
each segment's metadata carries `dump_id` plus whatever was passed to
`.with_metadata(...)`; if you want dump-aware filenames on disk,
insert a thin processor before `write_back()` that reads metadata and
renames. The disk-mode trade-offs in
[`Non-S3 dump key conventions`](#out-of-scope) apply.

```rust
let _guard = TracedRuntime::builder()
    .with_custom_pipeline(|p| p.symbolize().gzip().write_back())
    .with_trigger(rx)
    .build()?;

let receipt = control.dump_now().await?;
// receipt.dump_id is set; on-disk filenames follow the writer's
// existing rotation scheme unless a custom processor reads metadata.
```

### Time spans

Two methods cover the useful shapes:

```rust
// Whole ring at trigger time; resolve as soon as those segments
// finish the pipeline.
control.dump_now().await?;

// Look-back plus a post-trigger window. "-5 min .. +1 min": only
// segments whose `end_epoch >= now - 300s` are included from the
// ring, then keep capturing for another 60s before resolving.
control
    .dump_window(Some(Duration::from_secs(300)), Duration::from_secs(60))
    .await?;

// Whole ring plus a post-trigger window. `pre = None` means take
// everything the ring still has.
control
    .dump_window(None, Duration::from_secs(60))
    .await?;
```

The pre filter is best-effort: the ring keeps only what
`max_total_size` lets it keep, so the actual covered span (reported as
`receipt.time_range`) may be shorter than the requested `pre`. The
post window is wall-clock and starts at trigger time.

Both methods return a `DumpRun` that resolves to
`Result<DumpReceipt, DumpError>` when awaited. Chain
`.with_metadata(...)` before `.await` to attach correlation pairs that
get stamped onto each captured segment.

## Return value

`DumpReceipt` resolves once the last captured segment finishes the
pipeline. It carries everything the caller needs to find the dump later
or to log what landed:

| Field                | Meaning                                                                                                       |
| -------------------- | ------------------------------------------------------------------------------------------------------------- |
| `dump_id: DumpId`    | ULID minted at `send` time. Time-sortable. Shows up in S3 keys and in `dump-id` user metadata on each object. |
| `segments_processed` | Count of segments that made it through the pipeline.                                                          |
| `finished_at`        | When the last segment finished the pipeline.                                                                  |
| `time_range`         | Actual covered span. May be shorter than the requested `pre` if the ring did not retain that much history.    |

The trigger time itself (when `send` was called) is embedded in `dump_id`
and can be extracted via `DumpId`.

When the pipeline ends at S3, each dump materializes under its own prefix:

```
{prefix}/dumps/{dump_id}/{epoch_secs}-{index}.bin.gz
```

`ListObjectsV2 prefix={prefix}/dumps/` returns ULID-prefixed entries
sorted by time, and `ListObjectsV2 prefix={prefix}/dumps/{dump_id}/`
returns one dump.

Per-segment objects carry the same S3 user metadata as continuous-mode
uploads (`service`, `boot-id`, `segment-index`, `start-time`, `host`;
see `background_task/s3.rs`) plus `dump-id`. Callers that want
additional correlation pairs (e.g. a human-readable reason, an
incident id) pass them via `.with_metadata(...)` on the `DumpRun`;
pipeline stages decide what to do with them (the S3 stage can surface
them as additional user metadata, a custom redactor can read them, etc.).

| Field                        | Where it lives                                                |
| ---------------------------- | ------------------------------------------------------------- |
| `dump_id`                    | Key path; also `dump-id` S3 user metadata on every segment    |
| `triggered_at`               | Embedded in the ULID (`DumpId` can extract)                   |
| `service`, `boot_id`, `host` | Existing per-segment S3 user metadata, unchanged              |
| `segment.index`              | Key path; also `segment-index` S3 user metadata               |
| `segment.size_bytes`         | S3 object `ContentLength` (free)                              |
| `segment.start_epoch`        | Key path; also `start-time` S3 user metadata                  |
| `total_compressed_bytes`     | Sum of `ContentLength` from `ListObjectsV2`                   |
| Caller-supplied correlation  | `.with_metadata(...)`, stamped onto `SegmentData::metadata`   |

Completion is signalled in-process by `DumpReceipt` resolving. There is
no library-written completion marker in S3; applications that need a
cross-process signal publish it through whatever channel they already
use.

## What the library does for you

When the `DumpRun` future is awaited, the control mints a `DumpId`,
packs the timing parameters and any `with_metadata` entries into a
request, and forwards it to the worker over the trigger channel.

The worker stamps the following into each captured segment's
`SegmentData::metadata` before the pipeline runs:

- `dump_id` (always set on a triggered run)
- Anything from `.with_metadata(...)`

Pipeline stages read `SegmentData::metadata` the same way they already
read keys like `epoch_secs` and `content_encoding`. The S3 uploader
branches on `metadata.get("dump_id")`:

- Present: emit the dump-style key layout above and attach `dump-id`
  as per-object S3 user metadata.
- Absent (continuous mode): emit today's continuous-mode key,
  unchanged. The continuous layout, rationale, and partition strategy
  are documented in `s3-worker-design.md` section 2.

Nothing in the worker is S3-specific. The trigger feature stamps
metadata; the S3 uploader reads it. A custom redactor or a
`write_back()` stage sees the same metadata and can react however it
wants.

## Worker

The writer keeps producing sealed segments into the ring exactly as
today. `MemFs::seal` evicts the oldest segments on push when bytes
would exceed `max_total_size`. `DiskFs::seal` renames the active file
to its sealed name and lets the file accumulate on disk under the
writer's existing budget. Neither backend depends on the worker
running.

Without a trigger registered, `WorkerLoop::run` behaves as today: pop
segments from the ring as they appear, run each through the configured
processor chain, park on `Fs::wait_for_more` between cycles.

With `with_trigger(rx)` set, the same loop selects on:

- `self.stop` (existing `CancellationToken`, used on shutdown)
- `self.fs.writer_done` (existing, used to start drain-to-empty)
- the new `trigger_rx` populated by `TriggerControl::dump`

It does not call `take_files` between triggers. When a request
arrives, it stamps the metadata, takes the snapshot, runs each captured
segment through the same processor chain, and resolves the receipt
when the last one finishes. Then it re-parks.

Timing collapses to a single code path. At trigger time the worker
reads the current highest sealed segment index as `H`. The pre-set is
exactly `{ idx : idx <= H }`, additionally filtered to
`epoch >= now - pre` when `pre` is `Some`; the post-set is exactly
`{ idx : idx > H }` arriving before the `post` deadline. A single `u32`
high-water-mark replaces the more obvious `HashSet<SegmentIndex>`: no
allocation, no membership probe, identical correctness because writer
indices are monotonic.

`dump_now()` is equivalent to `dump_window(None, Duration::ZERO)`.
With `post == 0` the worker short-circuits the deadline so the future
resolves as soon as the pre-set finishes the pipeline. Otherwise it
arms `tokio::time::sleep(post)` and keeps popping `take_files` until
the deadline fires; the union of pre and post goes through the same
processor chain.

The only semantic difference between snapshot and windowed runs is
whether post-trigger arrivals are included. In memory mode, a windowed
run can lose post segments if `post` exceeds the ring's drain budget
(see "Memory-mode windowed loss" below); a snapshot run has no such
failure mode.

Overlapping windowed runs are not supported in v1: while one is in
flight, the worker is committed to a single `dump_id` stamp for
incoming segments, and a second windowed dump would force a per-segment
fan-out (which `dump_id` to write?). Calling `dump_window` with
`post > 0` while another windowed dump is in flight returns
`DumpError::InFlight`. `dump_now()` never claims future arrivals and is
not affected.

**Memory-mode windowed loss.** If `post` exceeds the ring's drain
budget, post-window segments may evict before the worker claims them.
This is a real loss, not a degraded-but-correct outcome. The doc
surfaces it explicitly; the mitigation is on the caller side, size
`max_total_size` for the worst expected `post`.

**Disk-mode behavior when parked.** Sealed files accumulate on disk
under the writer's existing budget. If the application never triggers,
the budget acts as a circular FIFO exactly as today when S3 is
unreachable (see `s3-worker-design.md` section 4 on disk-space safety).
The `max_total_size` knob is the lever.

### Open decision: how to handle `post` larger than the ring

The "Memory-mode windowed loss" callout above just warns the caller.
There are three structural ways to handle it more robustly. Picking one
(or layering them) is left open.

**Option 1 — Pin captured segments in the ring.** The worker marks
captured segments so the writer's eviction skips them.

- 1a. Live stream loses data when the pinned set fills the ring.
- 1b. Reserve a fraction of the ring for pinned segments; dump
  truncates when the reserve fills.

Pros: total memory stays bounded by `max_total_size`. Cons: writer's
eviction code learns about pinning; either live tracing has a new drop
path (1a) or the dump has a hidden hard cap (1b).

**Option 2 — Surface partial captures.** Nothing in the ring or the
writer changes. The worker counts capture targets that evicted before
pickup and reports them on the receipt:

```rust
pub struct DumpReceipt {
    pub dump_id: DumpId,
    pub segments_processed: usize,
    pub segments_lost: usize,           // new
    pub finished_at: SystemTime,
    pub time_range: (SystemTime, SystemTime),
}
```

Pros: zero change to ring/writer; honest about loss. Cons: caller has
to inspect the receipt; default behavior is still partial-loss-by-
silence unless the caller acts.

**Option 3 — Move captures into a side buffer.** On capture, the
worker moves the segment out of the ring into a dump-owned side buffer
(memory mode: `Arc` transfer; disk mode: rename to a staging
directory). The ring's budget drops immediately; the live stream is
isolated. The pipeline drains the side buffer at its own pace.

Pros: writer's eviction unchanged; live stream fully isolated; dump
completeness guaranteed. Cons: process memory/disk usage grows beyond
`max_total_size` during a dump. Optionally cap with a separate
`max_dump_holding_size`; overflow truncates the dump.

**Comparison.**

| Aspect              | Option 1 (pin)                             | Option 2 (surface) | Option 3 (side buffer)                          |
| ------------------- | ------------------------------------------ | ------------------ | ----------------------------------------------- |
| Dump completeness   | Guaranteed within a pin reserve            | Best-effort        | Guaranteed within an optional holding cap       |
| Live stream cost    | Possible drops (1a) or capped reserve (1b) | None               | None                                            |
| Memory/disk ceiling | Stays at `max_total_size`                  | Stays              | Grows during a dump                             |
| Writer changes      | Eviction consults a pin flag               | None               | None (capture moves segments out)               |
| Worker changes      | Pin/unpin on capture/release               | Counter + check    | Move-out on capture; own buffer and drain loop  |
| Caller burden       | None                                       | Inspect receipt    | None (unless cap is set and overflowed)         |

A natural combination is **Option 3 with an optional cap, plus
Option 2's `segments_lost` reporting when the cap is exceeded**. This
gives default-on completeness, an operator-controlled memory ceiling,
and honest reporting when the ceiling truncates a dump. Option 1 is
not subsumed and remains an alternative if "stay strictly within
`max_total_size`, no matter what" is a hard requirement.

Decision pending.

## API reference

```rust
pub mod trigger {
    pub fn channel() -> (TriggerControl, TriggerRx);
}

#[derive(Clone)]
pub struct TriggerControl { /* tx side of the trigger channel */ }

impl TriggerControl {
    /// Capture the ring as it is right now. Equivalent to
    /// `dump_window(None, Duration::ZERO)`.
    pub fn dump_now(&self) -> DumpRun<'_>;

    /// Capture the ring plus a post-trigger window. `pre` filters the
    /// pre-trigger segments to those with `end_epoch >= now - pre`;
    /// `None` takes whatever the ring still has. `post` is the
    /// wall-clock window of post-trigger arrivals to include.
    pub fn dump_window(&self, pre: Option<Duration>, post: Duration) -> DumpRun<'_>;
}

/// In-flight dump request. Resolves to `Result<DumpReceipt, DumpError>`
/// when awaited (via `IntoFuture`). Chain `.with_metadata(...)` before
/// awaiting to attach correlation pairs.
#[must_use = "DumpRun does nothing unless awaited"]
pub struct DumpRun<'a> { /* private */ }

impl<'a> DumpRun<'a> {
    /// Attach caller-supplied correlation pairs. Stamped onto every
    /// captured segment's `SegmentData::metadata` before the pipeline
    /// runs; pipeline stages decide what to do with them.
    pub fn with_metadata(self, metadata: HashMap<String, String>) -> Self;
}

impl<'a> IntoFuture for DumpRun<'a> {
    type Output = Result<DumpReceipt, DumpError>;
    type IntoFuture = /* boxed or named future */;
    fn into_future(self) -> Self::IntoFuture;
}

#[non_exhaustive]
pub struct DumpReceipt {
    pub dump_id: DumpId,
    pub segments_processed: usize,
    pub finished_at: SystemTime,
    pub time_range: (SystemTime, SystemTime),
}

#[non_exhaustive]
pub enum DumpError {
    /// A windowed dump is already running and v1 does not support overlap.
    InFlight,
    /// The worker is shutting down or already stopped.
    WorkerStopped,
    /// A pipeline stage failed on one of the captured segments.
    Pipeline(ProcessError),
}
```

`with_trigger(rx)` on `TracedRuntimeBuilder` writes a private
`Option<TriggerRx>`. Absence (the default) keeps today's continuous
behavior; presence flips the worker into triggered mode. `build()`
returns the existing `TelemetryGuard` regardless; no new axis on the
builder's phantom-state machinery.

Implementation note on the S3 uploader: `object_key` in
`background_task/s3.rs` (around line 150) switches on
`metadata.get("dump_id")`. When present it emits the dump-style key;
otherwise it falls through to today's continuous-mode layout. The
uploader knows nothing about the trigger feature directly; it reads
metadata, and the trigger feature is the source of that metadata.
`with_s3_uploader` itself is untouched, and continuous-mode uploads
never carry `dump_id`, so the continuous key layout stays exactly as
today.
