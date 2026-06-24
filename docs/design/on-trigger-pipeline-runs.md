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
method (`with_dump_trigger`), one cloneable control type reached through the
ambient runtime handle, and a `dump-id` metadata convention the S3 uploader
already knows how to read. Trace files stay in their normal location; the S3
stage additionally writes a small per-dump manifest under `dumps/` so a dump
is discoverable in a single GET.

## Wiring a trigger

`with_dump_trigger` is orthogonal to pipeline selection. Whichever pipeline
shape you would have wired for continuous mode, you keep, and adding
`with_dump_trigger(...)` flips that same pipeline into on-demand operation. The
default for most callers is the S3 preset: `with_s3_uploader(config)` builds
the standard `[Symbolize?, Gzip, S3]` pipeline and auto-populates writer
segment metadata.

The runtime mints the trigger channel internally. The application does not hand
a receiver in; it reaches the `DumpTrigger` through the ambient
`Dial9Handle::current()` from any thread the runtime owns (a monitor task, a
panic hook, a `/dump` handler, ...). No global plumbing.

```rust,ignore
use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::telemetry::Dial9Handle;

fn config() -> Dial9Config {
    Dial9Config::builder()
        .on_disk_buffer("/tmp/dial9/trace.bin")
        // Any pipeline you would run continuously. `with_dump_trigger`
        // only changes *when* it runs. Pass `|t| t.debounce(window)` to
        // coalesce a burst of triggers into one dump.
        .with_runtime(|r| r
            .with_s3_uploader(s3_config())
            .with_dump_trigger(|_| {}))
        .build_or_disabled()
}

#[dial9_tokio_telemetry::main(config = config)]
async fn main() {
    // Reach the trigger through the ambient handle from any runtime thread.
    let trigger = Dial9Handle::current()
        .dump_trigger()
        .expect("on-demand mode enabled");

    // Hand `trigger` to whatever subsystem decides when to dump (an
    // idle-ratio watcher, a panic hook, a `/dump` HTTP handler, ...).
    // `DumpTrigger` is `Clone`; share it freely.
}
```

`dump_trigger()` returns `Some` only when the runtime was built with
`with_dump_trigger`; in continuous mode it returns `None`. If no pipeline is
configured the worker never spawns and every dump resolves
`DumpError::WorkerStopped`.

## Requesting a dump

Two shapes cover the useful cases:

```rust,ignore
use std::time::Duration;

// Everything the ring still holds, right now. No forward window.
trigger.dump_current_data();

// A window around the trigger: the 5 minutes before it and the 5 minutes
// after. The look-back captures pre-trigger segments whose span reaches
// `now - 300s`; the look-forward keeps the dump open for 300s and captures
// segments as they seal. You can look back only as far as the ring still
// retains, and forward only as long as the process keeps running; both
// sides are best-effort (see "Best-effort semantics").
trigger.dump_time_range(Duration::from_secs(300), Duration::from_secs(300));

// Pure look-back (no forward window): pass a zero look-forward.
trigger.dump_time_range(Duration::from_secs(300), Duration::ZERO);
```

`dump_current_data` and `dump_time_range` build a `DumpRun`. The request is
**dispatched when the run is dropped or awaited, whichever comes first** (so
`.with_metadata(...)` can mutate it before it is sent). In the temporary form
above (the run is a statement, dropped at the end of it) dispatch happens right
there. If you bind the run to a variable, dispatch waits until that binding is
awaited or goes out of scope. A look-back dump runs as soon as the matching
ring segments finish; a dump with a look-forward stays open until its forward
deadline elapses. Await the returned run only when you want the receipt:

```rust,ignore
let receipt = trigger
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

```rust,ignore
trigger
    .dump_current_data()
    .with_metadata("reason", "panic")
    .with_metadata("incident", incident_id);
```

## Coalescing duplicate triggers (debounce)

A single source often re-trips: a watcher that checks an idle ratio every poll,
a hot path that dumps on every slow request. Left alone, each trip starts a new
dump. `with_dump_trigger(|t| t.debounce(window))` installs a leading-edge gate:
the first trigger in a quiet period dispatches normally, and any trigger
arriving within `window` of it folds into that dump instead of starting a new
one, resolving `DumpError::Coalesced { into }` (where `into` names the dump it
folded into).

```rust,ignore
match trigger.dump_current_data().with_metadata("reason", "idle-drop").await {
    Ok(receipt) => { /* this trip started the dump */ }
    Err(DumpError::Coalesced { into }) => {
        // A near-simultaneous trip already covers this; `into` is its id.
    }
    Err(e) => { /* WorkerStopped or Pipeline */ }
}
```

The gate lives on the trigger stored in the session, so every `dump_trigger()`
clone shares it; the effective rate is at most one dump per `window` across all
callers. The window is measured from the last accepted request and is not
extended by coalesced ones, so a whole burst folds into the first dump. There
is no coordination without `debounce`: unrelated subsystems can each dump
without stepping on one another (a *cooldown* that rejects extra triggers
outright, rather than folding them, is a possible future addition).

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

(With no configured prefix the key is `dumps/{dump_id}.json`.)

The manifest is the index for the dump. It lists the keys of every trace object
the dump produced, plus the dump's identity (`dump_id`, `triggered_at`,
`time_range`, `segments_processed`, and any caller `with_metadata(...)` pairs).
The `time_range` is the dump's **actual covered span** (the earliest captured
segment's creation second to the latest captured segment's seal second), which
can be narrower than the requested window:

```json
{
  "dump_id": "01J9Z...",
  "triggered_at": "2026-06-09T14:30:42Z",
  "time_range": ["2026-06-09T14:26:11Z", "2026-06-09T14:34:58Z"],
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

A dump that captures some segments while others fail terminally still resolves
`Ok`; `receipt.segments_processed` counts only the survivors. `Err` is reserved
for `WorkerStopped`, `Coalesced`, and total `Pipeline` failure (every captured
segment failed and nothing landed).

## Custom pipelines

`with_custom_pipeline(|p| ...)` gives you the full chain. The trigger feature
is pipeline-agnostic: it stamps dump metadata onto each captured segment before
any stage runs, and stages decide what to do with it.

**Custom pipeline ending at S3.** Equivalent to the preset but you control
the exact stage list (omit symbolize, add a redactor, etc.). The `s3()` stage
attaches the `dump-id` user metadata the same way the preset does.

```rust,ignore
let config = Dial9Config::builder()
    .on_disk_buffer("/tmp/dial9/trace.bin")
    .with_runtime(|r| r
        .with_custom_pipeline(|p| p.symbolize().redact(my_redactor).gzip().s3(s3_config()))
        .with_dump_trigger(|_| {}))
    .build_or_disabled();

// `trigger.dump_current_data()` / `trigger.dump_time_range(..)` behave
// identically to the S3-preset example.
```

Unlike the preset, the custom path does not auto-populate writer segment
metadata. If you want identity entries (service, host, etc.) embedded in
trace files, call `with_segment_metadata(...)` explicitly.

**Custom pipeline ending at `write_back()` (no S3).** Dumps to disk under the
writer's directory. The receipt still carries `dump_id`, and each segment's
metadata carries the dump ids plus whatever was passed to `.with_metadata(...)`;
if you want dump-aware filenames on disk, insert a thin processor before
`write_back()` that reads metadata and renames. There is no manifest in this
mode: the manifest is written by the `s3()` stage, so pipelines that do not end
at S3 get only the per-segment dump metadata (`receipt.manifest_key` is
`None`).

```rust,ignore
let config = Dial9Config::builder()
    .on_disk_buffer("/tmp/dial9/trace.bin")
    .with_runtime(|r| r
        .with_custom_pipeline(|p| p.gzip().write_back())
        .with_dump_trigger(|_| {}))
    .build_or_disabled();

// let receipt = trigger.dump_current_data().await?;
// receipt.dump_id is set; on-disk filenames follow the writer's existing
// rotation scheme unless a custom processor reads metadata.
```

A runnable version of each shape ships under `examples/`:
`on_trigger_dump.rs` (disk, `dump_current_data` + debounce) and
`on_trigger_dump_windows.rs` (disk, `dump_time_range` look-back/look-forward +
concurrent overlapping dumps).

## Return value

Awaiting a dump is optional. The dump is dispatched when the `DumpRun` is
dropped or awaited; awaiting only retrieves the `DumpReceipt`. For a look-back
dump it resolves once the last captured segment finishes the pipeline; for a
dump with a look-forward it resolves after the forward deadline elapses and the
last in-window segment finishes. It carries everything the caller needs to find
the dump later or to log what landed:

| Field                       | Meaning                                                                                                                                                                                                                                                             |
| --------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `dump_id: DumpId`           | ULID minted when the dump is dispatched. Time-sortable. Surfaces as `dump-id` user metadata on each S3 object.                                                                                                                                                      |
| `segments_processed: usize` | Count of segments that made it through the pipeline (survivors only).                                                                                                                                                                                               |
| `finished_at: SystemTime`   | When the last segment finished the pipeline.                                                                                                                                                                                                                        |
| `time_range`                | Actual covered span, which can extend past `triggered_at` when a look-forward was requested. May be shorter than the requested window on either side: look-back if the ring did not retain that much history, look-forward if the dump stopped before the deadline. |
| `manifest_key`              | `Some({prefix}/dumps/{dump_id}.json)` when the pipeline ends at S3; `None` otherwise (no manifest off S3).                                                                                                                                                          |

The trigger time itself is embedded in `dump_id` and can be extracted via
`DumpId::timestamp()`.

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
| `triggered_at`               | Embedded in the ULID (`DumpId::timestamp()`); also in the manifest                           |
| `time_range`                 | Manifest field; also `receipt.time_range`                                                    |
| `segments` (object keys)     | Manifest `segments` array                                                                    |
| `service`, `boot_id`, `host` | Existing per-segment S3 user metadata, unchanged                                             |
| `segment.index`              | Key path; also `segment-index` S3 user metadata                                              |
| `segment.size_bytes`         | S3 object `ContentLength` (free)                                                             |
| `segment.start_epoch`        | Key path; also `start-time` S3 user metadata                                                 |
| Caller-supplied correlation  | `.with_metadata(...)`, stamped onto segment metadata; also in the manifest `metadata`        |
| Manifest                     | `{prefix}/dumps/{dump_id}.json`; also `receipt.manifest_key`                                 |

Completion is signalled in-process by the dump's future resolving. For S3
pipelines the manifest doubles as the cross-process signal: its presence at
`{prefix}/dumps/{dump_id}.json` means the dump finished writing and lists what
landed (a failed dump writes none). Off-S3 pipelines have no manifest, so
applications that need a cross-process signal there publish it through whatever
channel they already use (incidents-table row, Slack message, etc.).

## What the library does for you

When you call `dump_current_data` or `dump_time_range`, the trigger mints a
`DumpId`, packs the look-back and look-forward (either may be zero) and any
`with_metadata` entries into a request, and (on drop/await) forwards it to the
worker over the trigger channel. Awaiting the returned run is optional and only
retrieves the receipt.

The worker stamps the following onto each captured segment's metadata before
the pipeline runs:

- `dump_id`: a comma-joined list of every active dump the segment matched (a
  segment in two forward windows gets both ids).
- Each `.with_metadata(key, value)` pair, namespaced as `dump.{key}`. When a
  segment matches multiple dumps with the same key, the first-registered dump
  wins.

Pipeline stages read this metadata the same way they already read keys like
`epoch_secs` and `content_encoding`. The S3 uploader checks
`metadata.get("dump_id")`:

- Present: attach `dump-id` (hyphen) as per-object S3 user metadata
  (comma-joined when the segment belongs to more than one dump), and emit each
  `dump.{key}` pair as user metadata with the `dump.` prefix stripped (pairs
  that are not valid S3 user metadata, or that collide with the reserved keys,
  are skipped with a rate-limited warning). The key layout is today's
  continuous-mode layout, unchanged. The uploader also records the key it just
  wrote against each of those dump ids so it can build their manifests later.
- Absent (continuous mode): emit today's continuous-mode object, unchanged.

The manifest's `metadata` map holds the raw caller keys (un-namespaced, as
passed to `with_metadata`), not the `dump.`-prefixed segment keys.

When a dump completes, the worker signals every stage in pipeline order via
`SegmentProcessor::finalize_dump`, handing it a `DumpCompletion`. The S3 stage
uses that to PUT `{prefix}/dumps/{dump_id}.json` from the keys it accumulated
for that id. The same key may appear in several manifests when it was captured
by overlapping forward windows. `finalize_dump` runs for every resolved dump
(errored and empty ones included) so stages always get to clear per-dump
bookkeeping; a failed dump still gets the signal but the S3 stage writes no
manifest for it.

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

With a trigger set, the same loop selects on:

- `self.stop` (existing `CancellationToken`, used on shutdown)
- `self.fs.writer_done` (existing, used to start drain-to-empty)
- the new trigger receiver populated by `DumpTrigger`
- a deadline branch for the nearest open forward window

It does not call `take_files` between triggers. When a request arrives, it
registers the dump with its window `[trigger - lookback, trigger +
lookforward]` and a deadline at `trigger + lookforward`. It then drains the
ring with `take_files_matching(windows)`, and for each segment it pops it
attaches the segment to every active dump whose window covers the segment's
`[creation, seal]` span, stamps the metadata, and runs it through the same
processor chain. A pure look-back dump (zero look-forward) has its window fully
in the past, so it resolves as soon as the matching ring segments finish. A
dump with a look-forward stays registered until wall-clock passes its deadline,
at which point it stops accepting segments and its receipt resolves once the
last in-window segment finishes.

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

```rust,ignore
// Builder method (on the runtime sub-builder reached via `with_runtime`).
impl TracedRuntimeBuilder {
    /// Flip the worker into on-demand operation. The trigger is reached at
    /// runtime through `Dial9Handle::dump_trigger()`. Configure coalescing
    /// with `DumpTriggerConfig::debounce`.
    pub fn with_dump_trigger<F>(self, configure: F) -> Self
    where
        F: FnOnce(&mut DumpTriggerConfig);
}

// Reaching the trigger at runtime.
impl Dial9Handle {
    /// The on-demand dump trigger for this runtime, or `None` when the
    /// runtime was built without `with_dump_trigger`. Cheap to clone.
    pub fn dump_trigger(&self) -> Option<DumpTrigger>;
}

#[derive(Debug, Default, Clone)]
pub struct DumpTriggerConfig { /* private */ }

impl DumpTriggerConfig {
    pub fn new() -> Self;
    /// Coalesce duplicate triggers within `window` into a single dump.
    pub fn debounce(&mut self, window: Duration);
}

#[derive(Debug, Clone)]
pub struct DumpTrigger { /* clone of the tx side + shared debounce gate */ }

impl DumpTrigger {
    /// Capture everything the ring still holds, right now. No forward window.
    pub fn dump_current_data(&self) -> DumpRun<'_>;

    /// Capture the window `[trigger - lookback, trigger + lookforward]`. Either
    /// side may be `Duration::ZERO`. Never errors and never resizes or pins the
    /// ring; the actual covered span is reported on `DumpReceipt::time_range`.
    pub fn dump_time_range(&self, lookback: Duration, lookforward: Duration) -> DumpRun<'_>;
}

/// In-flight dump request. Dispatched when this run is dropped or awaited,
/// whichever comes first (so `with_metadata` can mutate it before send).
/// Dropping does not cancel the dump. Resolves to `Result<DumpReceipt,
/// DumpError>` via `IntoFuture`.
pub struct DumpRun<'a> { /* private */ }

impl<'a> DumpRun<'a> {
    /// Attach a caller-supplied correlation pair. Chainable. Each pair is
    /// stamped onto every captured segment's metadata (namespaced `dump.{key}`)
    /// before the pipeline runs.
    pub fn with_metadata(self, key: impl Into<String>, value: impl Into<String>) -> Self;
}

impl<'a> IntoFuture for DumpRun<'a> {
    type Output = Result<DumpReceipt, DumpError>;
    /* ... */
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DumpId(/* ulid::Ulid */);

impl DumpId {
    /// The instant the dump was triggered, embedded in the id.
    pub fn timestamp(&self) -> SystemTime;
}
// Also: `Display` (Crockford base32) and `FromStr`.

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
    /// Every captured segment failed in a pipeline stage (total failure).
    Pipeline(ProcessErrorKind),
    /// The trigger was coalesced into an in-flight dump by the debounce gate.
    /// No new dump ran; `into` names the dump that covers this trigger.
    Coalesced { into: DumpId },
}
```

`with_dump_trigger` mints the trigger channel and stashes the `DumpTrigger` in
the session's shared state (a `OnceLock`); the receiver is plumbed to the worker
as a private `Option<DumpRx>`. Absence (the default) keeps today's continuous
behavior; presence flips the worker into triggered mode. `build` returns the
existing guard regardless; no new axis on the builder's phantom-state
machinery.

Implementation note on the S3 uploader: `object_key` in
`background_task/s3.rs` is unchanged; both modes produce the same key layout
for trace objects. The dump-specific behavior is additive: when
`metadata.get("dump_id")` is present, the uploader attaches `dump-id` as
per-object S3 user metadata and records the written key against the `dump_id`;
on the worker's `finalize_dump` signal it PUTs `{prefix}/dumps/{dump_id}.json`.
The uploader knows nothing about the trigger feature directly; it reads metadata
and reacts to a completion signal, and the trigger feature is the source of
both. `with_s3_uploader` itself is untouched.
