# On-Trigger Pipeline Runs

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
method (`with_trigger(rx)`), one cloneable control type, and a `dump-id`
metadata convention the S3 uploader already knows how to read. Trace files
stay in their normal location; the S3 stage additionally writes a small
per-dump manifest under `dumps/` so a dump is discoverable in a single GET.

## Wiring a trigger

`with_trigger(rx)` is orthogonal to pipeline selection. Whichever pipeline
shape you would have wired for continuous mode, you keep, and adding
`with_trigger(rx)` flips that same pipeline into on-demand operation. The
default for most callers is the S3 preset: `with_s3_uploader(config)` builds
the standard `[Symbolize?, Gzip, S3]` pipeline and auto-populates writer
segment metadata.

```rust
use dial9::dump;

let (control, rx) = dump::trigger();

let _guard = TracedRuntime::builder()
    .with_s3_uploader(s3_config.clone())
    .with_trigger(rx)
    .build()?;

// Hand `control` to whatever subsystem decides when to dump
// (an idle-ratio watcher, a panic hook, a `/dump` HTTP handler, ...).
// `DumpControl` is `Clone`; share it freely.
```

## Requesting a dump

Two shapes cover the useful cases:

```rust
use std::time::Duration;

// Everything the ring still holds, right now. No forward window.
control.dump_current_data();

// A window around the trigger: the 5 minutes before it and the 5 minutes
// after. The look-back captures pre-trigger segments whose `end_epoch >=
// now - 300s`; the look-forward keeps the dump open for 300s and captures
// segments as they seal. You can look back only as far as the ring still
// retains, and forward only as long as the process keeps running; both
// sides are best-effort (see "Best-effort semantics").
control.dump_time_range(Duration::from_secs(300), Duration::from_secs(300));

// Pure look-back (no forward window): pass a zero look-forward.
control.dump_time_range(Duration::from_secs(300), Duration::ZERO);
```

Both dispatch the moment you call them; you do not have to await anything
for the dump to run. A look-back dump runs immediately; a dump with a
look-forward stays open until its forward deadline elapses. Await the
returned handle only when you want the receipt:

```rust
let receipt = control
    .dump_time_range(Duration::from_secs(300), Duration::from_secs(300))
    .with_metadata("reason", "idle-ratio-drop")
    .await?;

tracing::info!(
    dump_id = %receipt.dump_id,
    segments = receipt.segments_processed,
    "dump complete",
);
```

`with_metadata` is chainable; call it once per pair to attach correlation
data to every captured segment:

```rust
control
    .dump_current_data()
    .with_metadata("reason", "panic")
    .with_metadata("incident", incident_id);
```

## Finding a dump in S3

Dumped trace objects land in the **same S3 location** as continuous-mode
uploads, under today's key layout, each carrying a `dump-id` value as S3 user
metadata (the ULID minted at trigger time, also returned on the receipt). A
segment that falls inside the forward windows of several concurrent dumps
carries all of their ids as a comma-joined `dump-id` value, and its key appears
in each of those dumps' manifests. The trace files are never relocated.

On top of that, the S3 stage writes one **manifest** object per dump at:

```
{prefix}/dumps/{dump_id}.json
```

The manifest is the index for the dump. It lists the keys of every trace object
the dump produced, plus the dump's identity (`dump_id`, `triggered_at`,
`time_range`, `segments_processed`, and any caller `with_metadata(...)` pairs):

```json
{
  "dump_id": "01J9Z...",
  "triggered_at": "2026-06-09T14:30:42Z",
  "time_range": ["2026-06-09T14:25:42Z", "2026-06-09T14:35:42Z"],
  "segments_processed": 12,
  "metadata": { "reason": "idle-ratio-drop" },
  "segments": [
    "traces/2026-06-09/1425/checkout-api/i-0abc/1741384200-1.bin.gz",
    "traces/2026-06-09/1430/checkout-api/i-0abc/1741384542-3.bin.gz"
  ]
}
```

Because `dump_id` is a ULID, discovery is two cheap steps:

- `ListObjectsV2 prefix={prefix}/dumps/` returns every dump's manifest, sorted
  by time (ULIDs are time-sortable). No bucket traversal, no `HeadObject` fan-out.
- `GetObject {prefix}/dumps/{dump_id}.json` returns one dump's full set of trace
  keys, ready to fetch.

This keeps the trace files where an operator would expect them during normal
operation (continuous-mode tooling and lifecycle policies apply unchanged) while
still giving "which files belong to this dump?" a one-GET answer. The original
design relocated dump objects under a `dumps/{dump_id}/` prefix; this manifest
approach points at the files instead of moving them.

## Best-effort semantics

Both sides of a dump are strictly best-effort. There is no ring resizing, no
segment pinning, and no duplication of buffered data. If the requested window
cannot be fully covered, the dump gets whatever survived and the application
keeps running.

The look-back is bounded by what the ring retained. The ring keeps only what
`max_total_size` lets it keep, so a `lookback` wider than the retained history
simply captures the segments that survived; under upload pressure the oldest
part of a wide look-back can be evicted before the worker reaches it. The
actual covered span is reported as `receipt.time_range`. This is never an
error and never resizes or pins the ring. Size `max_total_size` for the
history depth you expect to need.

The look-forward is bounded by the process lifetime and by the same ring
budget. A forward window keeps the dump open until `triggered_at +
lookforward`, capturing segments as they seal and flow through the pipeline.
Captured segments are uploaded as they are popped, exactly like continuous
mode, so if uploads lag the producer a forward segment can be evicted before
the worker pops it. Nothing is held back or pre-allocated for the forward
window; it is the same seal/pop/upload path, gated by a deadline.

## Custom pipelines

`with_custom_pipeline(|p| ...)` gives you the full chain. The trigger feature
is pipeline-agnostic: it stamps `dump_id` onto segment metadata before any
stage runs, and stages decide what to do with it.

**Custom pipeline ending at S3.** Equivalent to the preset but you control
the exact stage list (omit symbolize, add a redactor, etc.). The `s3()` stage
attaches the `dump-id` user metadata the same way the preset does.

```rust
let _guard = TracedRuntime::builder()
    .with_custom_pipeline(|p| p.symbolize().redact(my_redactor).gzip().s3(s3_config.clone()))
    .with_trigger(rx)
    .build()?;

// `control.dump_current_data()` / `control.dump_time_range(..)` behave
// identically to the S3-preset example.
```

Unlike the preset, the custom path does not auto-populate writer segment
metadata. If you want identity entries (service, host, etc.) embedded in
trace files, call `with_segment_metadata(...)` explicitly.

**Custom pipeline ending at `write_back()` (no S3).** Dumps to disk under the
writer's directory. The receipt still carries `dump_id`, and each segment's
metadata carries `dump_id` plus whatever was passed to `.with_metadata(...)`;
if you want dump-aware filenames on disk, insert a thin processor before
`write_back()` that reads metadata and renames. There is no manifest in this
mode: the manifest is written by the `s3()` stage, so pipelines that do not end
at S3 get only the per-segment `dump_id` metadata (`receipt.manifest_key` is
`None`).

```rust
let _guard = TracedRuntime::builder()
    .with_custom_pipeline(|p| p.symbolize().gzip().write_back())
    .with_trigger(rx)
    .build()?;

let receipt = control.dump_current_data().await?;
// receipt.dump_id is set; on-disk filenames follow the writer's existing
// rotation scheme unless a custom processor reads metadata.
```

## Return value

Awaiting a dump is optional. The dump is dispatched when you call
`dump_current_data` or `dump_time_range`; awaiting the returned handle only
retrieves the `DumpReceipt`. For a look-back-only dump it resolves once the
last captured segment finishes the pipeline; for a dump with a look-forward it
resolves after the forward deadline elapses and the last in-window segment
finishes. It carries everything the caller needs to find the dump later or to
log what landed:

| Field                | Meaning                                                                                                                                                                                                                                                             |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `dump_id: DumpId`    | ULID minted when the dump is dispatched. Time-sortable. Surfaces as `dump-id` user metadata on each S3 object.                                                                                                                                                      |
| `segments_processed` | Count of segments that made it through the pipeline.                                                                                                                                                                                                                |
| `finished_at`        | When the last segment finished the pipeline.                                                                                                                                                                                                                        |
| `time_range`         | Actual covered span, which can extend past `triggered_at` when a look-forward was requested. May be shorter than the requested window on either side: look-back if the ring did not retain that much history, look-forward if the dump stopped before the deadline. |
| `manifest_key`       | `Some({prefix}/dumps/{dump_id}.json)` when the pipeline ends at S3; `None` otherwise (no manifest off S3).                                                                                                                                                          |

The trigger time itself is embedded in `dump_id` and can be extracted via
`DumpId`.

When the pipeline ends at S3, each object carries the same S3 user metadata
as continuous-mode uploads (`service`, `boot-id`, `segment-index`,
`start-time`, `host`; see `background_task/s3.rs`) plus `dump-id`. Callers
that want additional correlation pairs (a human-readable reason, an incident
id) pass them via `.with_metadata(...)`; pipeline stages decide what to do
with them (the S3 stage surfaces them as additional user metadata, a custom
redactor can read them, etc.).

| Field                        | Where it lives                                                                               |
| ---------------------------- | -------------------------------------------------------------------------------------------- |
| `dump_id`                    | `dump-id` S3 user metadata on every object; also the manifest filename                       |
| `triggered_at`               | Embedded in the ULID (`DumpId` can extract); also in the manifest                            |
| `time_range`                 | Manifest field; also `receipt.time_range`                                                    |
| `segments` (object keys)     | Manifest `segments` array                                                                    |
| `service`, `boot_id`, `host` | Existing per-segment S3 user metadata, unchanged                                             |
| `segment.index`              | Key path; also `segment-index` S3 user metadata                                              |
| `segment.size_bytes`         | S3 object `ContentLength` (free)                                                             |
| `segment.start_epoch`        | Key path; also `start-time` S3 user metadata                                                 |
| Caller-supplied correlation  | `.with_metadata(...)`, stamped onto `SegmentData::metadata`; also in the manifest `metadata` |
| Manifest                     | `{prefix}/dumps/{dump_id}.json`; also `receipt.manifest_key`                                 |

Completion is signalled in-process by `DumpReceipt` resolving. For S3
pipelines the manifest doubles as the cross-process signal: its presence at
`{prefix}/dumps/{dump_id}.json` means the dump finished writing and lists what
landed. Off-S3 pipelines have no manifest, so applications that need a
cross-process signal there publish it through whatever channel they already use
(incidents-table row, Slack message, etc.).

## What the library does for you

When you call `dump_current_data` or `dump_time_range`, the control mints a
`DumpId`, packs the look-back and look-forward (either may be zero) and any
`with_metadata` entries into a request, and forwards it to the worker over the
trigger channel immediately. Awaiting the returned handle is optional and only
retrieves the receipt.

The worker stamps the following into each captured segment's
`SegmentData::metadata` before the pipeline runs:

- `dump_id` (always set on a triggered run; carries every active dump the
  segment matched, so a segment in two forward windows gets both ids)
- Anything from `.with_metadata(...)`

Pipeline stages read `SegmentData::metadata` the same way they already read
keys like `epoch_secs` and `content_encoding`. The S3 uploader checks
`metadata.get("dump_id")`:

- Present: attach `dump-id` as per-object S3 user metadata (comma-joined when
  the segment belongs to more than one dump). The key layout is today's
  continuous-mode layout, unchanged. The uploader also records the key it just
  wrote against each of those dump ids so it can build their manifests later.
- Absent (continuous mode): emit today's continuous-mode object, unchanged.

When a dump completes, the S3 stage writes its manifest. The worker already
tracks dump completion (it is what resolves `DumpReceipt` once the last captured
segment finishes the pipeline); on completion it signals the terminal stage to
finalize that dump, and the S3 stage PUTs `{prefix}/dumps/{dump_id}.json` from
the keys it accumulated for that id. The same key may appear in several
manifests when it was captured by overlapping forward windows. The exact
finalize-hook signature is an implementation detail for the code PR; the
contract is "worker says the dump is done, the S3 stage flushes its manifest."

Nothing in the worker is S3-specific. The trigger feature stamps metadata and
signals completion; the S3 stage owns both the per-object `dump-id` tag and the
manifest. A custom redactor or a `write_back()` stage sees the same metadata and
the same completion signal and can react however it wants (off-S3 stages simply
write no manifest).

## Worker

The writer keeps producing sealed segments into the ring exactly as today.
`MemFs::seal` evicts the oldest segments on push when bytes would exceed
`max_total_size`. `DiskFs::seal` renames the active file to its sealed name
and lets the file accumulate on disk under the writer's existing budget.
Neither backend depends on the worker running.

Without a trigger registered, `WorkerLoop::run` behaves as today: pop
segments from the ring as they appear, run each through the configured
processor chain, park on `Fs::wait_for_more` between cycles.

With `with_trigger(rx)` set, the same loop selects on:

- `self.stop` (existing `CancellationToken`, used on shutdown)
- `self.fs.writer_done` (existing, used to start drain-to-empty)
- the new `trigger_rx` populated by `DumpControl`

It does not call `take_files` between triggers. When a request arrives, it
registers the dump with its window `[trigger - lookback, trigger +
lookforward]` and a deadline at `trigger + lookforward`. It then drains the
ring as usual, and for each segment it pops it attaches the segment to every
active dump whose window covers the segment's epoch, stamps the metadata, and
runs it through the same processor chain. A pure look-back dump (zero
look-forward) has its window fully in the past, so it resolves as soon as the
matching ring segments finish. A dump with a look-forward stays registered:
the select loop gains a deadline branch, and the dump keeps collecting
newly-sealed segments until wall-clock passes its deadline, at which point it
stops accepting segments and its receipt resolves once the last in-window
segment finishes.

Because a forward window claims future arrivals, a single newly-sealed segment
can fall inside the windows of several concurrent dumps. The worker therefore
associates each popped segment with _all_ active dumps it matches, rather than
a single owner. Dumps still run concurrently with no contention; the only
shared effect is that one segment may be recorded against more than one
`dump_id` (see "Finding a dump in S3"). A dump that requests more history than
the ring holds, or whose forward segments evict under upload pressure, simply
captures what survived, with no error and no effect on the live stream.

**Disk-mode behavior when parked.** Sealed files accumulate on disk under the
writer's existing budget. If the application never triggers, the budget acts
as a circular FIFO exactly as today when S3 is unreachable (see
`s3-worker-design.md` section 4 on disk-space safety). The `max_total_size`
knob is the lever.

## API reference

```rust
pub mod dump {
    /// Create a dump control + receiver pair; pass the receiver to
    /// `with_trigger(rx)`.
    pub fn trigger() -> (DumpControl, DumpRx);
}

#[derive(Clone)]
pub struct DumpControl { /* tx side of the trigger channel */ }

impl DumpControl {
    /// Capture everything the ring still holds, right now. No forward window.
    pub fn dump_current_data(&self) -> DumpRun<'_>;

    /// Capture the window `[trigger - lookback, trigger + lookforward]`. Either
    /// side may be `Duration::ZERO`.
    ///
    /// `lookback` captures pre-trigger segments with `end_epoch >= trigger -
    /// lookback`; you can look back only as far as the ring still retains, so a
    /// `lookback` wider than the retained history is best-effort and captures
    /// what survived. `lookforward` keeps the dump open until `trigger +
    /// lookforward`, attaching segments as they seal; it is uncapped and bounded
    /// only by the process lifetime, and is best-effort under upload pressure.
    /// The actual covered span is reported on `DumpReceipt::time_range`. This
    /// never errors and never resizes or pins the ring.
    pub fn dump_time_range(&self, lookback: Duration, lookforward: Duration) -> DumpRun<'_>;
}

/// In-flight dump request. The dump is dispatched when `dump_current_data` /
/// `dump_time_range` is called; this handle is only needed to retrieve the
/// receipt. Resolves to `Result<DumpReceipt, DumpError>` when awaited (via
/// `IntoFuture`). For a look-forward dump the future resolves after the forward
/// deadline elapses. Chain `.with_metadata(...)` before awaiting to attach
/// correlation pairs. Dropping the handle does not cancel the dump.
pub struct DumpRun<'a> { /* private */ }

impl<'a> DumpRun<'a> {
    /// Attach a caller-supplied correlation pair. Chainable. Each pair is
    /// stamped onto every captured segment's `SegmentData::metadata` before
    /// the pipeline runs; pipeline stages decide what to do with them.
    pub fn with_metadata(self, key: impl Into<String>, value: impl Into<String>) -> Self;
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
    /// `Some({prefix}/dumps/{dump_id}.json)` when the pipeline ends at S3;
    /// `None` for off-S3 terminals (no manifest is written there).
    pub manifest_key: Option<String>,
}

#[non_exhaustive]
pub enum DumpError {
    /// The worker is shutting down or already stopped.
    WorkerStopped,
    /// A pipeline stage failed on one of the captured segments.
    Pipeline(ProcessError),
}
```

`with_trigger(rx)` on `TracedRuntimeBuilder` writes a private
`Option<DumpRx>`. Absence (the default) keeps today's continuous behavior;
presence flips the worker into triggered mode. `build()` returns the existing
`TelemetryGuard` regardless; no new axis on the builder's phantom-state
machinery.

Implementation note on the S3 uploader: `object_key` in
`background_task/s3.rs` (around line 150) is unchanged; both modes produce
the same key layout for trace objects. The dump-specific behavior is additive:
when `metadata.get("dump_id")` is present, the uploader attaches `dump-id` as
per-object S3 user metadata and records the written key against the `dump_id`;
on the worker's end-of-dump signal it PUTs `{prefix}/dumps/{dump_id}.json`. The
uploader knows nothing about the trigger feature directly; it reads metadata and
reacts to a completion signal, and the trigger feature is the source of both.
`with_s3_uploader` itself is untouched.
