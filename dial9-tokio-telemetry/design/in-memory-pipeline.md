# In-memory pipeline

> **Status: design, not yet implemented.**

## Overview

Let dial9 run with no filesystem dependency. Users with plenty of RAM, or running in environments where disk is unavailable or unwelcome, get a memory-only path: encoded bytes flow from the writer through an in-process queue to the worker, then through the existing `SegmentProcessor` pipeline to whichever destination they configured.

The existing lifecycle is disk-mediated end to end. Flush thread writes encoded batches into `trace.N.bin.active`, writer renames to `trace.N.bin` on rotation, worker polls the directory every second, reads the file back into memory and runs the processor pipeline.

A single `pub(crate)` `Fs` enum covers the writer + worker boundary, with one variant per backend. `Fs::Disk` wraps `std::fs` for the rotating disk path. `Fs::Mem` keeps active bytes in a per-path `Vec<u8>` map and routes sealed bytes through an internal channel to the worker. It covers the full segment lifecycle: write-side (`create`, `seal`, `remove_*`) and read-side (`take_files`, `wait_for_more`, `writer_done`, `mark_writer_done`). Not a trait: the set is closed (only these two) and never exposed publicly, so an enum beats a trait that would only ever be implemented twice.

**Core principle:** the disk path stays the default and unchanged. Memory mode is one constructor swap on the existing writer builder.

## Goals

- A memory-only path that never touches the real filesystem for the segment data itself. Encoded bytes flow writer, in-process queue, worker, destination.
- Reuse the existing `SegmentProcessor` pipeline. No new API for users who already wire `.s3(...)` or custom processors.
- Bounded memory with the same "drop oldest, never block the recorder" policy the disk path already uses for eviction. Memory mode evicts at ring overflow on a slot-bounded ring (drop-at-seal via `force_push`). Disk mode evicts after seal (`evict_oldest` in the writer).

## Non-goals

- Recovering in-flight segments after a process crash. Anything in memory at crash time is gone (the disk path already drops segments under eviction pressure).
- Spill-to-disk fallback when the memory budget is exhausted. A follow-up could make this configurable, but avoiding disk altogether keeps the feature usable in environments where disk access is unavailable.
- Single-file mode analogue (`RotatingWriter::single_file`) under `MemFs`. Memory mode always rotates.

---

## 1. Architecture

```
Application threads                Worker thread (dedicated current_thread rt)
─────────────────────              ─────────────────────────────────────────
record_event() ──┐
                 ▼
          ThreadLocalBuffer
                 │ flush
                 ▼
        CentralCollector (existing)
                 │ drain
                 ▼
          RotatingWriter
            │
            ├── Arc<Fs> ── full segment lifecycle
            │   (DiskFs: real files | MemFs: per-path Vec<u8> + channel)
            │
            └── on seal:
                ├── disk: file is now on the filesystem ──▶ worker calls
                │       fs.take_files() (claim + lazy load)
                │
                └── memory: push (index, Bytes) into the ──▶ worker calls
                        internal channel + Notify              fs.take_files()
                                                                   │
                                                                   ▼
                                                              Pipeline (SegmentProcessor chain):
                                                                SymbolizeProcessor
                                                                GzipCompressor
                                                                S3PipelineUploader / user processor
```

The hot path (recording, thread-local flush, central collector) does not change. The work is on three pieces:

- A new `Fs` enum with `Disk` and `Mem` variants.
- The in-pipeline carrier (`SegmentRef` enum so processors operate in both modes).
- Backpressure observability symmetric across both backends (`WorkerCycleMetrics`).

### The `Fs` enum

```rust
pub(crate) enum Fs {
    Disk(DiskFs),
    Mem(MemFs),
}

/// Active-segment write handle. One variant per backend so `create`
/// returns a concrete type.
pub(crate) enum ActiveHandle {
    Disk(std::fs::File),
    Mem(MemActiveWriter),
}
// impl Write for ActiveHandle { ... match self ... }

impl Fs {
    // --- Write side. Each method is a two-arm match on `self`. ---
    fn create(&self, path: &Path) -> io::Result<ActiveHandle>;
    /// Seal the active segment. Disk: rename to the path produced by
    /// stripping the `.active` suffix. Memory: drain the active buffer
    /// and push to the internal channel.
    fn seal(&self, active: &Path, index: u32) -> io::Result<SegmentRef>;
    /// Remove a sealed segment. `reason` separates writer backpressure
    /// eviction (counts toward `dropped_segments`) from worker terminal
    /// cleanup after a non-retryable failure (does not, since that is a
    /// processing failure tracked via `SegmentProcessMetrics`). Disk:
    /// unlinks the sealed file plus extension-renamed siblings (e.g.
    /// `.gz` from `WriteBackProcessor`), drops the claim entry, and
    /// bumps `dropped_segments` if `reason == Eviction`. Memory: no-op.
    fn remove_sealed(&self, seg: &SegmentRef, reason: RemoveReason) -> io::Result<()>;
    fn remove_active(&self, path: &Path) -> io::Result<()>;

    // --- Read side ---
    /// Return claim receipts for newly visible sealed segments plus
    /// backpressure gauges sampled from the same scan. Each segment is
    /// handed out at most once per `Fs` value (see claim-set dedup
    /// below).
    fn take_files(&self) -> TakenFiles;
    /// Wait for new segments to potentially be available. Disk: sleeps
    /// `poll_interval`. Memory: awaits the channel `Notify`.
    async fn wait_for_more(&self, stop: &CancellationToken, poll_interval: Duration);
    /// Writer signaled it will produce no more segments. Memory: channel
    /// flag. Disk: always false (stop token handles shutdown).
    fn writer_done(&self) -> bool;
    /// Writer finalize hook. Memory: sets writer_done + notify. Disk:
    /// no-op.
    fn mark_writer_done(&self);
}

pub(crate) enum RemoveReason {
    /// Writer shed this segment for backpressure (`evict_oldest`).
    /// Counts toward `dropped_segments`.
    Eviction,
    /// Worker removed this after a terminal pipeline state
    /// (non-retryable failure, retry-budget exhaustion, panic).
    /// Not a backpressure drop.
    Terminal,
}
```

`DiskFs` and `MemFs` stay as the per-variant payload structs holding the backend-specific state. Inherent methods on `Fs` are a two-arm match that forwards to the active variant: static dispatch, branch-predicted, inlinable, no vtable.

`TakenFiles` carries the dispensed claims plus per-cycle gauges (`ring_depth`, `ring_bytes`, `in_flight_count`, `in_flight_bytes`, `dropped_segments`). One scan per worker cycle drives both work and observability (see section 6 for metrics).

- **`DiskFs`** holds the segment directory and stem at construction and maintains a `HashMap<u32, u64>` of claimed indices to uncompressed sizes (under a `Mutex`). `take_files` scans the directory, stats each unclaimed file **outside the claim-set mutex**, then re-acquires the mutex once to insert all new entries. Keeping `metadata()` syscalls off the locked path matters because the writer's `evict_oldest` takes the same mutex (via `remove_sealed`). The `std::fs::read` for the payload itself is deferred to `TakenSegment::load`.
- **`MemFs`** holds active-only state (per-path `Vec<u8>` map) plus the internal writer-to-worker ring (`BoundedQueue` of `max_segments` slots). Sealed bytes do not live in `MemFs`'s active map. `seal` takes the `Vec<u8>`, wraps it as `Bytes` (zero-copy) and `force_push`es into the ring, which evicts the oldest slot when full (see section 3). `take_files` pops **at most one** segment from the ring per call and returns it with payload already in hand. `remove_sealed` is a no-op: bytes already left `MemFs`.

### At-most-once handoff and lazy load

Without dedup, a stalled downstream causes the disk worker to re-read every accumulated `.bin` file every scan. `take_files` dispenses each file at most once per `Fs` value. With the read deferred to `TakenSegment::load`, peak in-flight memory stays at one segment regardless of backlog.

`load()` returns `Err(NotFound)` when the file vanished between scan and load (last-stage cleanup raced the worker). The worker logs a rate-limited warn and skips. The next scan prunes the claim entry.

Memory mode dedups by construction, each ring slot pops exactly once.

### Worker loop

The worker holds an `Arc<Fs>` and runs one loop:

```rust
loop {
    let r = self.fs.take_files();
    self.emit_cycle_metrics(&r);
    self.process_segments(r.segments).await;
    if stop || self.fs.writer_done() {
        // A push can land between an empty take_files and this
        // check, so re-poll and drain to empty before exiting.
        loop {
            let r = self.fs.take_files();
            self.emit_cycle_metrics(&r);
            if r.segments.is_empty() {
                return;
            }
            self.process_segments(r.segments).await;
        }
    }
    if r.segments.is_empty() {
        self.fs.wait_for_more(&stop, self.poll_interval).await;
    }
}
```

### `RotatingWriter`

Holds `Arc<Fs>`. Disk constructors build `Fs::Disk(DiskFs::from_base_path(&trace_path))`. The memory constructor builds `Fs::Mem(MemFs::with_capacity(...))`, which owns the internal writer-to-worker channel. The recorder builder hands the worker the same `Arc<Fs>`.

Eviction lives where it is natural:

- **Disk:** writer holds `closed_files: VecDeque<(SegmentRef, u64)>`. After every rotation, `evict_oldest` pops the front and calls `Fs::remove_sealed(seg, RemoveReason::Eviction)` until the byte budget is satisfied. The `Eviction` reason bumps `dropped_segments`. Worker terminal cleanup calls `remove_sealed(seg, RemoveReason::Terminal)`, which unlinks without counting (a processing failure, not backpressure).
- **Memory:** the ring is slot-bounded by `max_segments`. Push via `force_push` drops the oldest slot when full. The user-facing peak is `max_segment_size * max_segments`, with eviction triggered by slot count, segments smaller than `max_segment_size` (low-traffic time-based rotation) under-fill the budget but never evict early. `closed_files` stays empty.

### Rejected alternatives

- **`MemoryWriter` as a sibling `TraceWriter` impl.** Duplicates encoder/rotation/metadata/drain-timer logic. Tests still need `TempDir`. A single `RotatingWriter<Mode>` is simpler.
- **Split write-side and read-side enums.** Disk read-side is stateful (claim-set dedup) so the state would have to be shared across both anyway. One `Fs` keeps the lifecycle in one place.
- **Eager payload load in `take_files`.** Reads every unclaimed file into RAM on each scan. First drain after boot or recovery scales with backlog size. Lazy `TakenSegment::load` bounds peak in-flight memory to one segment.
- **`Fs` as a trait.** `Arc<dyn Fs>` would add a vtable hop per call plus a boxed `Write` handle and a boxed wait future, pure overhead on the `MemFs` hot path. A trait with associated types (`Self::Writer`, `async fn`) avoids the boxing but still carries a type parameter that would thread through the worker. The enum is static, branch-dispatched, and parameter-free.
- **Sync `mpsc<()>(1)` for wakeup.** Already shuttle-shimmed, but blocking `recv()` would stall the current-thread worker, `spawn_blocking` per wait churns the thread pool.
- **`Mutex<VecDeque<MemSealedSegment>>` for queue.** Adds a sync mutex on a path other crate queues keep lock-free.
- **`crossbeam_queue::SegQueue`.** Unbounded, no eviction primitive.
- **`tokio::sync::mpsc::Receiver` carrying segments.** Built-in wakeup but no eviction, needs a side channel for overflow.
- **Writer-to-worker handoff as a continuous byte stream.** Breaks per-segment metrics, S3 multipart upload, retry semantics, the rotation contract, forces every processor to handle partial bytes.
- **Unified evict-after-seal for both modes.** Loses memory mode's natural ring byte bound and forces a sealed-state mutex.

---

## 2. Pipeline integration

Memory mode reuses the existing pipeline.

The only obstacle is `SegmentData::segment`, today typed as `SealedSegment { path, index }` and filesystem-coupled. Memory mode has no real path. `SealedSegment` stays as the disk variant, paired with a new `MemorySegment`, both under a `SegmentRef` enum:

```rust
pub struct SealedSegment { pub path: PathBuf, pub index: u32 }
pub struct MemorySegment { pub index: u32, pub size: u64 }

pub enum SegmentRef {
    Disk(SealedSegment),
    Memory(MemorySegment),
}

impl SegmentRef {
    pub fn index(&self) -> u32 { ... }
    /// Human-readable id for tracing and metric labels.
    /// Disk renders the path, memory renders `mem://{index}`.
    pub fn display_id(&self) -> impl std::fmt::Display + '_ { ... }
}
```

`SegmentData::segment()` returns `&SegmentRef`. Disk-only call sites (`WriteBackProcessor`, the path-logging branch of `S3PipelineUploader`) match on the enum. `WriteBackProcessor` paired with a memory writer is a compile error via the `PipelineBuilder` typestate (see section 4), so the disk-only `match` arm is defense in depth.

> **Breaking change.** `SegmentData::segment()` return type goes from `&SealedSegment` to `&SegmentRef`. External `SegmentProcessor` impls that called `data.segment().path()` need to match on the enum or read via `data.payload()`. Internal processors are updated in this change.

### Failure modes: retry budget, panic

Apply to every pipeline stage (memory or disk). Worker wraps each processor's `process(...)` future:

- **Bounded retry budget.** On `retryable: true`, the worker re-runs the stage up to `retry_budget` times before dropping the segment. Default 3. `S3PipelineUploader` wraps its own `CircuitBreaker` and is instantiated with `retry_budget = 1`. Memory mode needs this since segments leave the ring on pop. Without bounded retry, the first transient error would lose the segment.
- **Panic isolation.** Stage future runs inside `AssertUnwindSafe(...).catch_unwind()`. A panic drops the segment, fires metrics and keeps the stage instance (so transient state like connection pools survives). **Contract for custom processor authors:** a panicked `process(...)` must leave the instance valid for the next call (no held locks, no half-filled buffers, no corrupted state).

These attach to processors via `PipelineBuilder::pipe_with_config`.

---

## 3. Memory story

### What gets allocated

Three byte pools contribute to peak working set:

| Bucket | Owner | Size | Notes |
|--------|-------|------|-------|
| Active segment buffer | flush thread | up to `max_segment_size` | `Vec<u8>` inside the encoder. `RawEncoder` is a thin wrapper |
| Queued sealed segments | `BoundedQueue` ring | up to `max_segment_size * max_segments` | `MemSealedSegment { index, bytes: Bytes }` per slot. `Bytes` is a zero-copy Arc clone. Slot-bounded: `force_push` evicts oldest on overflow |
| In-flight pipeline data | worker | up to one `max_segment_size` + transient stage growth | Pipeline runs serial: at most one segment in-flight. Disk path bounds via lazy `TakenSegment::load`, memory via pop-one in the `Fs::Mem` arm of `take_files` (section 1). `SymbolizeProcessor` (cpu-profiling only) appends a symbol-table chunk. `GzipCompressor` output is a fraction of input. Benches will surface actual ratios. |

Note on interning: per-batch string interning lives on the recording thread, not the writer. Each `ThreadLocalBuffer` resets its encoder on every flush (`Encoder::reset_to_infallible`), so the interner peak is bounded by one batch encode, not a segment's life.

### Peak working set

The slot-bounded ring caps queued bytes at `max_segment_size * max_segments`. In-flight bytes are separate (the pipeline is serial, so at most one segment plus stage-internal growth is held outside the queue at any moment).

**Peak memory contract:** `max_segment_size * max_segments (queue) + max_segment_size (in-flight) + max_segment_size (active buffer)`. Add another `max_segment_size` in-flight when `cpu-profiling` is enabled, since `SymbolizeProcessor` appends a symbol-table chunk.

Pick `max_segments` so the worker can absorb a 5-10x burst of slowness before drops fire. With 1 MB segments at 60s rotation that means 10–15 slots. Steady state is ~3 MB (one active, one in-flight, a small queue).


### Comparison to the disk path

Disk already pays for the active buffer and one in-flight segment in RAM (lazy `TakenSegment::load` keeps it at one regardless of backlog). Memory mode adds the ring itself, up to `max_segment_size * max_segments`, in process heap instead of as `.bin` files on disk. Encoded bytes are identical. Eviction fires under the same "worker can't keep up" trigger: disk via `evict_oldest`, memory via slot-bounded `force_push`.

### Byte accounting and drop-oldest

Memory-mode gauges on the `MemFs` channel:

- **`ring_depth`**: segments waiting in the ring after the current cycle's pop.
- **`in_flight_bytes`**: current payload bytes summed across in-flight segments. Tracks real RAM (symbolize grows, gzip shrinks) because the worker re-balances it via `SegmentAccounting::adjust` after each successful stage.
- **`in_flight_count`**: matching segment-count gauge.
- **`dropped_segments`**: cumulative segments shed by ring-slot eviction.

The writer is the sole eviction source. The worker only consumes. Concurrent `queue.pop()` between writer eviction and worker consumption is safe because `BoundedQueue` is MPMC-safe, so each slot pops exactly once.

**Accounting spec.** `SegmentData` carries an `accounting: Option<SegmentAccounting>` populated only for memory-backed segments:

```rust
struct SegmentAccounting {
    in_flight_bytes: Arc<AtomicU64>,
    in_flight_count: Arc<AtomicU64>,
    size: u64, // last observed payload.len()
}

impl SegmentAccounting {
    fn adjust(&mut self, new_size: u64) {
        // fetch_add or fetch_sub the delta, then update self.size.
    }
}

impl Drop for SegmentAccounting {
    fn drop(&mut self) {
        self.in_flight_bytes.fetch_sub(self.size, Ordering::AcqRel);
        self.in_flight_count.fetch_sub(1, Ordering::AcqRel);
    }
}
```

The worker calls `acct.adjust(data.payload.len())` after each successful stage. Failure branches let `Drop` run with whatever size the last successful `adjust` set, so the gauge stays balanced no matter where the segment terminated. Stage-internal scratch (encoder buffers, etc.) is invisible, only `data.payload` bytes are tracked.

Disk-backed segments hold `accounting: None`. Drop fires regardless of pipeline outcome: success, retry-budget exhaustion, non-retryable failure, and panic all converge on `SegmentData::drop`. Panic case: the panicked future owns `SegmentData` and drops it inside `catch_unwind`, so `SegmentAccounting::drop` runs with the size last set by the most recent successful stage. The synthetic `SegmentData` the worker emits for panic metrics carries `accounting: None`. In-flight segments are out of the ring once popped, so eviction can't double-count them.

Drop signals: `dropped_segments` counter increments per evicted segment (see section 6 metrics catalog), plus a rate-limited `tracing::warn!` so sustained overruns are visible in logs.

---

## 4. API surface

Memory mode is one constructor swap. The writer carries its `Arc<Fs>` and the runtime builder spawns the worker from it.

```rust
let writer = RotatingWriter::in_memory(
    /* max_segment_size */ 1 * MB,
    /* max_segments     */ 16,
)?;
let (runtime, guard) = TracedRuntime::builder()
    .with_s3_uploader(s3_config)
    .build_and_start(tokio_builder, writer)?;
```

### Mode handling

`RotatingWriter` carries a phantom `Mode` typestate: `RotatingWriter<Disk>` (default) or `RotatingWriter<Memory>`.

```rust
pub struct RotatingWriter<Mode = Disk> { /* ... */ }
pub trait WriterMode: sealed::Sealed {}
pub struct Disk;
pub struct Memory;
impl WriterMode for Disk {}
impl WriterMode for Memory {}
```

Constructors split:

- `new`, `single_file`, `builder` return `RotatingWriter<Disk>`.
- `in_memory` returns `RotatingWriter<Memory>`.

Builder constraint:

- `HasTracePath::build_and_start(rt, w: RotatingWriter<Disk>)`. Memory writer here is a compile error.
- `NoTracePath::build_and_start(rt, w: RotatingWriter<Memory>)`. Memory-mode entry point.
- `NoTracePath::build_and_start_with_writer(rt, w: impl TraceWriter)` keeps the generic path for `NullWriter`, custom writers and test doubles. Small breaking change: `NullWriter` / test-double callers that previously invoked `build_and_start` migrate to `build_and_start_with_writer`.

`Mode` is `PhantomData` at runtime. Storage is mode-agnostic: the writer holds `Arc<Fs>` regardless, the phantom only drives the compile-time builder constraints above. A `tracks_closed_files: bool` field controls whether the writer runs `evict_oldest` after rotation (disk only). `finalize` unconditionally calls `Fs::mark_writer_done`, whose `Disk` arm is a no-op so disk mode pays nothing.

#### Preventing misuse

Pipeline mode is also enforced at compile time via `PipelineBuilder<DiskReq = NoDiskRequired>`. `.write_back()` transitions to `PipelineBuilder<RequiresDisk>`. Memory-writer entry points constrain `DiskReq = NoDiskRequired`, so attaching `WriteBackProcessor` to a memory writer is a compile error. User-supplied processors that need disk backing opt in via a `DiskBoundProcessor` marker trait and use `pipe_disk_bound(p)` instead of `pipe(p)`.

### `RotatingWriter::in_memory` defaults

- `rotation_period`: reuses disk's `DEFAULT_ROTATION_PERIOD` (60 s). Caps how long an event can sit in the active buffer when traffic is low.
- `drain_interval`: internal.
- ring slot count: exactly `max_segments`. The user-facing budget is slots, not bytes, small segments under-fill the budget but never trigger early eviction.

`in_memory` returns `Err(InvalidInput)` if `max_segment_size == 0` or `max_segments == 0`.

### Rejected alternatives

- **`(max_segment_size, max_total_size)` byte budget.** Becomes `slot_cap = max_total_size / max_segment_size` internally, a slot budget dressed up as bytes, and the truncating division silently shaves the user's stated cap. Slot-count is what eviction acts on, so expose that directly.
- **`take_pending_receiver` on `TraceWriter` plus writer-held `pending_receiver: Option<ChannelReceiver>`.** Trait method overridden only on `RotatingWriter` plus a writer field drained once at build time. `MemFs` owns the channel and the builder hands the worker the concrete `Arc<MemFs>` directly.
- **`MemoryPipeline { writer, receiver }` newtype.** Same problem: an extra public type users carry around for a transient setup step.
- **`builder.with_memory_writer(seg, total)` high-level method.** Loses writer-level config knobs (`rotation_period` etc.) at the builder level or forces per-knob builder methods.

---

## 5. Crash and shutdown semantics

Graceful shutdown for memory mode mirrors disk:

1. Stop the flush thread.
2. `RotatingWriter::finalize`: seal the active segment via `Fs::seal` (`MemFs` pushes onto the ring) and signal `writer_done`. The worker observes `writer_done` on its next wait and exits after one final drain.
3. `TelemetryGuard::graceful_shutdown(timeout)` waits for the worker to exit.

Disk follows the same steps without `writer_done`. The worker's stop-token plus the drain-until-empty loop handles shutdown.

**Ordering invariant:** Three sync points, the final segment is always delivered before the worker exits:

1. **Writer seal:** `MemFs::seal_handle` invokes `BoundedQueue::force_push`, which establishes a Release synchronization point on the queued slot. Eviction of an oldest slot (when the ring is full) only displaces stale entries and does not gate the just-pushed slot.
2. **Writer mark-done:** after the final `seal`, `RotatingWriter::finalize` calls `Fs::mark_writer_done`. On `MemFs` this does `writer_done.store(true, Release)` then `notify_one`.
3. **Worker observe + re-drain:** the memory arm of `wait_for_more` does `writer_done.load(Acquire)`. Once it observes `true` the worker exits the wait and the run loop drains in a loop, calling `take_files` until the ring is empty. Each `BoundedQueue::pop` performs the `Acquire` on its slot. Every queued segment is consumed because the drain loop does not stop until `take_files` reports empty, and the final pushed segment is visible because the write to `writer_done` (Release) synchronizes-with the load (Acquire) and the queue push happens-before the `mark_done` store on the writer thread.

Lost-wakeup avoidance is handled by the standard `notified() / enable() / re-check writer_done / await` pattern in the memory arm of `wait_for_more`. The future is registered before the second `writer_done` load so any `notify_one` that fires between the first load and the await becomes the permit the await consumes.

---

## 6. Testing

Reuse the patterns from the disk path. The headline change is that **`RotatingWriter` tests can construct `Fs::Mem` directly as the backing store**, dropping `TempDir` for existing cases.

**Unit coverage.** Each `Fs` arm (active map transitions, seal pushes and drains correctly, `Bytes` zero-copy), channel ordering and accounting, claim-set dedup on `Fs::Disk` (each file dispensed at most once), lazy payload (`TakenSegment::load` defers reads, NotFound between scan and load yields `Err`), writer rotation/eviction/metadata against `Fs::Mem` (small disk-only subset stays on `Fs::Disk` for rename atomicity and dir-scan edge cases).

**Integration.** Memory `RotatingWriter` paired with a test `SegmentProcessor` that captures `SegmentData`. Run a workload, confirm bytes decode and event counts match. S3 integration against `s3s` (already used by the disk-path S3 tests).

**Stress + Shuttle.** High segment-emission rate against a slow downstream processor: verify drop-oldest fires, counters increment, writer never blocks. Shuttle scenarios: concurrent `force_push` / `pop`, shutdown race on `writer_done`, eviction-under-contention accounting, `SegmentData` drop racing with `force_push` eviction (no underflow).

### Memory regression tests

Memory mode keeps bytes in process heap that disk kept on disk. Regressions are easier to introduce and harder to spot. Two layers to be checked on PRs:

- **Counter assertions.** Fixed-seed workload against a memory `RotatingWriter`. Assert `ring_depth` stays within `max_segments`, `in_flight_bytes` stays within `max_segment_size` (plus a margin when `cpu-profiling` symbolizes), eviction fires when the slot budget is intentionally tight, queue depth stays bounded. Catches accounting regressions.
- **Heap baseline.** Same workload under a deterministic heap profiler (e.g. `dhat`), baseline checked into CI. Catches leaks and allocator-side regressions.

### Metrics

`FlushMetrics`, `SegmentProcessMetrics` and `TlDrainMetrics` cover writer, flush and per-segment paths as today. New: a per-cycle `WorkerCycleMetrics` entry (`Operation::WorkerCycle`) emitted once per `take_files` call, symmetric across disk and memory:

- `RingDepth` (memory only): segments waiting in the ring after this cycle's pop. The primary "near eviction" gauge.
- `RingBytes`: emitted as `None` today since the memory backend tracks slot counts, not ring bytes.
- `InFlightCount` / `InFlightBytes`: segments claimed but not yet released by last-stage cleanup or `remove_sealed`. Memory updates `InFlightBytes` after every successful stage via `SegmentAccounting::adjust`, so the gauge follows payload growth (symbolization) and shrinkage (compression). Rising values mean the pipeline is not shedding work fast enough.
- `DroppedSegments`: backend-side evictions. Disk: `remove_sealed(_, Eviction)` from `evict_oldest`. Memory: ring slot eviction via `force_push`.
- `SegmentsDispatched`: segments handed into the pipeline this cycle.

Fires every cycle, drained-empty included, so a stuck pipeline shows climbing `InFlightBytes` with `SegmentsDispatched == 0`. `ChannelReceiver` keeps direct accessors for at-cadence sampling.

---

## 7. Open questions

**Adaptive sizing:** The current design takes a static `max_segments`. We could measure ring depth and let the writer grow/shrink within a configured range, similar to how some allocators auto-tune their arenas. Container users could benefit from reading cgroup `memory.max` / `memory.high` as an outer cap. Initially keeping it out of scope for v1 or at least until implementation testing shows workload data. The metrics catalog (`RingDepth`, `InFlightBytes`, `DroppedSegments`) covers the internal inputs we'd need.
