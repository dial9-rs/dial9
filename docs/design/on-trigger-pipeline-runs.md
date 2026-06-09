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
metadata convention the S3 uploader already knows how to read.

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

// Everything the ring still holds, right now.
control.dump_now();

// The last 5 minutes of history: only segments whose
// `end_epoch >= now - 300s`. You can look back as far as the ring
// buffer still retains; a `lookback` wider than the retained history
// just captures everything the ring has (see "Best-effort semantics").
control.dump_since(Duration::from_secs(300));
```

Both dispatch the moment you call them; you do not have to await anything
for the dump to run. Await the returned handle only when you want the
receipt:

```rust
let receipt = control
    .dump_since(Duration::from_secs(300))
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
    .dump_now()
    .with_metadata("reason", "panic")
    .with_metadata("incident", incident_id);
```

## Finding a dump in S3

Dumped objects land in the **same S3 location** as continuous-mode uploads,
under today's key layout. The only difference is a `dump-id` value attached
as S3 user metadata on every object in the dump (the ULID minted at trigger
time, also returned on the receipt). To retrieve a dump's objects, filter on
that tag. There is no separate `dumps/` prefix and no manifest file; the
trace files are exactly where they would be during normal operation.

## Best-effort semantics

A dump captures only data the ring still holds at trigger time, on a strict
best-effort basis. There is no forward window, no ring resizing, no segment
pinning, and no duplication of buffered data. If a requested look-back is
wider than the ring retained, the dump gets whatever survived and the
application keeps running.

The look-back is bounded by what the ring retained. The ring keeps only what
`max_total_size` lets it keep, so `dump_since(d)` with a `d` wider than the
retained history simply returns the segments that survived; the actual
covered span is reported as `receipt.time_range`. This is never an error and
never resizes or pins the ring. Size `max_total_size` for the history depth
you expect to need.

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

// `control.dump_now()` / `control.dump_since(..)` behave identically to the
// S3-preset example.
```

Unlike the preset, the custom path does not auto-populate writer segment
metadata. If you want identity entries (service, host, etc.) embedded in
trace files, call `with_segment_metadata(...)` explicitly.

**Custom pipeline ending at `write_back()` (no S3).** Dumps to disk under the
writer's directory. The receipt still carries `dump_id`, and each segment's
metadata carries `dump_id` plus whatever was passed to `.with_metadata(...)`;
if you want dump-aware filenames on disk, insert a thin processor before
`write_back()` that reads metadata and renames.

```rust
let _guard = TracedRuntime::builder()
    .with_custom_pipeline(|p| p.symbolize().gzip().write_back())
    .with_trigger(rx)
    .build()?;

let receipt = control.dump_now().await?;
// receipt.dump_id is set; on-disk filenames follow the writer's existing
// rotation scheme unless a custom processor reads metadata.
```

## Return value

Awaiting a dump is optional. The dump is dispatched when you call `dump_now`
or `dump_since`; awaiting the returned handle only retrieves the
`DumpReceipt`, which resolves once the last captured segment finishes the
pipeline. It carries everything the caller needs to find the dump later or to
log what landed:

| Field                | Meaning                                                                                                       |
| -------------------- | ------------------------------------------------------------------------------------------------------------- |
| `dump_id: DumpId`    | ULID minted when the dump is dispatched. Time-sortable. Surfaces as `dump-id` user metadata on each S3 object. |
| `segments_processed` | Count of segments that made it through the pipeline.                                                          |
| `finished_at`        | When the last segment finished the pipeline.                                                                  |
| `time_range`         | Actual covered span. May be shorter than the requested look-back if the ring did not retain that much history. |

The trigger time itself is embedded in `dump_id` and can be extracted via
`DumpId`.

When the pipeline ends at S3, each object carries the same S3 user metadata
as continuous-mode uploads (`service`, `boot-id`, `segment-index`,
`start-time`, `host`; see `background_task/s3.rs`) plus `dump-id`. Callers
that want additional correlation pairs (a human-readable reason, an incident
id) pass them via `.with_metadata(...)`; pipeline stages decide what to do
with them (the S3 stage surfaces them as additional user metadata, a custom
redactor can read them, etc.).

| Field                        | Where it lives                                                |
| ---------------------------- | ------------------------------------------------------------- |
| `dump_id`                    | `dump-id` S3 user metadata on every object in the dump        |
| `triggered_at`               | Embedded in the ULID (`DumpId` can extract)                   |
| `service`, `boot_id`, `host` | Existing per-segment S3 user metadata, unchanged              |
| `segment.index`              | Key path; also `segment-index` S3 user metadata               |
| `segment.size_bytes`         | S3 object `ContentLength` (free)                              |
| `segment.start_epoch`        | Key path; also `start-time` S3 user metadata                  |
| Caller-supplied correlation  | `.with_metadata(...)`, stamped onto `SegmentData::metadata`   |

Completion is signalled in-process by `DumpReceipt` resolving. There is no
library-written completion marker in S3; applications that need a
cross-process signal publish it through whatever channel they already use
(incidents-table row, Slack message, etc.).

## What the library does for you

When you call `dump_now` or `dump_since`, the control mints a `DumpId`, packs
the look-back (if any) and any `with_metadata` entries into a request, and
forwards it to the worker over the trigger channel immediately. Awaiting the
returned handle is optional and only retrieves the receipt.

The worker stamps the following into each captured segment's
`SegmentData::metadata` before the pipeline runs:

- `dump_id` (always set on a triggered run)
- Anything from `.with_metadata(...)`

Pipeline stages read `SegmentData::metadata` the same way they already read
keys like `epoch_secs` and `content_encoding`. The S3 uploader checks
`metadata.get("dump_id")`:

- Present: attach `dump-id` as per-object S3 user metadata. The key layout is
  today's continuous-mode layout, unchanged.
- Absent (continuous mode): emit today's continuous-mode object, unchanged.

Nothing in the worker is S3-specific. The trigger feature stamps metadata;
the S3 uploader reads it. A custom redactor or a `write_back()` stage sees the
same metadata and can react however it wants.

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
takes a snapshot of the segments currently in the ring (optionally filtered to
`end_epoch >= now - lookback`), stamps the metadata, runs each captured
segment through the same processor chain, and resolves the receipt when the
last one finishes. Then it re-parks.

Because a dump never claims future arrivals, there is no contention over the
`dump_id` stamp: dumps may overlap freely and run concurrently. A dump that
requests more history than the ring holds simply captures what is there, with
no error and no effect on the live stream.

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
    /// Capture everything the ring still holds, right now.
    pub fn dump_now(&self) -> DumpRun<'_>;

    /// Capture the last `lookback` of history: pre-trigger segments with
    /// `end_epoch >= now - lookback`.
    ///
    /// You can look back only as far as the ring buffer still retains.
    /// `lookback` is capped, in effect, by `max_total_size`: a window
    /// wider than the retained history is best-effort and simply captures
    /// every segment the ring still holds. The actual covered span is
    /// reported on `DumpReceipt::time_range`. This never errors and never
    /// resizes or pins the ring.
    pub fn dump_since(&self, lookback: Duration) -> DumpRun<'_>;
}

/// In-flight dump request. The dump is dispatched when `dump_now` /
/// `dump_since` is called; this handle is only needed to retrieve the
/// receipt. Resolves to `Result<DumpReceipt, DumpError>` when awaited (via
/// `IntoFuture`). Chain `.with_metadata(...)` before awaiting to attach
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
the same key layout. The only dump-specific behavior is that, when
`metadata.get("dump_id")` is present, the uploader attaches `dump-id` as
per-object S3 user metadata. The uploader knows nothing about the trigger
feature directly; it reads metadata, and the trigger feature is the source of
that metadata. `with_s3_uploader` itself is untouched.
