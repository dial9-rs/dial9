# ADR-0003: Disk retention is owned by `DiskFs`, hard-capped, and `ENOSPC` wipes and disables

- **Status:** accepted
- **Date:** 2026-06-10

## Context

dial9 bounds on-disk trace data with a logical byte budget (`max_total_size`).
In the shipped design (0.3.x and current `main`) retention has structural
problems that let the disk fill and cleanup stop:

- **Two half-owners, no durable truth.** The *writer* owns the eviction
  decision (`SegmentWriter::evict_oldest` over an in-memory `closed_files`
  list), but `DiskFs` owns the directory and the only accurate file sizes.
  The two disagree.
- **Eviction only runs while the writer is healthy.** `evict_oldest` is
  called from `rotate()` (and `finalize()`), so it stops the moment the
  writer latches `WriterState::Finished` — which is exactly what a hard IO
  error (e.g. `ENOSPC` on `create_segment`) does. Cleanup dies precisely when
  the disk is under pressure.
- **Stale accounting.** `closed_files` records each segment's *uncompressed
  seal-time* size, but the worker later rewrites segments to smaller
  `.bin.gz`. The writer's notion of "bytes used" drifts from reality.
- **Write-back bypasses `DiskFs`.** `WriteBackProcessor` uses `std::fs`
  directly to create the `.bin.gz` and delete the `.bin`, so the one
  component that owns the directory is blind to the single largest size
  change in the system (often a 5–10× shrink), and write-back races eviction.
- **A logical budget oblivious to real disk.** If `max_total_size` exceeds
  free space (the 1 GiB default on a small/shared volume), or a neighbor
  fills the volume, the device fills *before* the logical threshold triggers
  eviction; the writer then hits `ENOSPC`, latches `Finished`, and never
  recovers or reclaims — holding a full disk for the rest of the process.

This ADR redesigns disk retention. It is disk-only; the in-memory backend
(`MemFs`) already enforces the same contract by construction via its bounded
ring and is unchanged. (The separate background-worker CPU-spin bug under
persistent upload failure is tracked independently; it is not this ADR.)

## Decision

Make `DiskFs` — the component that owns the directory and runs continuously —
the single owner of retention.

1. **Single owner.** Remove the writer's `closed_files` / `evict_oldest`. The
   writer only *notifies* `DiskFs` on seal. `DiskFs` owns all accounting and
   eviction.

2. **Exact model + reconciliation scan.** `DiskFs` keeps an exact in-memory
   model of bytes per **segment family** (the `.bin` plus any derived
   artifact such as `.bin.gz`), because it becomes the chokepoint for every
   sealed-artifact mutation: seal reports size, **write-back is routed through
   `DiskFs`** (no direct `std::fs` in `WriteBackProcessor`), removal subtracts.
   A directory **scan, folded into the worker's per-cycle `take_files`**, is
   the authority that reconciles the model against reality and emits
   `reconcile_drift_bytes`. The model is the fast path used between scans
   (notably by seal-time eviction on the writer thread); the scan keeps it
   honest across crashes, operator/container deletion, and any future bypass.

3. **Two co-primary eviction triggers, independent of writer state.**
   - **(a)** the **worker run loop, every cycle** — reclamation that outlives
     `Finished` (the worker is the one component still alive afterward);
   - **(b)** **immediately after each seal, before opening the next active
     file** (writer thread) — a growth bound that holds even if the worker is
     jammed, since growth requires sealing.
   No dedicated watchdog thread: the only state both triggers miss
   simultaneously ("writer `Finished` *and* worker jammed") is provably
   non-growing, because a `Finished` writer does not write.

4. **The budget is the single ceiling, with reserved headroom for the active
   segment.** No on-disk file — sealed or active — and no family total may
   exceed `max_total_size`. Because `DiskFs` cannot observe the active
   segment growing (the writer writes straight to the file handle), we make
   the invariant hold *by construction*: `DiskFs` evicts sealed families down
   to `max_total_size − max_file_size`, reserving one `max_file_size` slice
   for the active segment. The worker-loop evictor therefore needs no
   knowledge of live active size.

5. **Eviction policy: oldest-first, in-flight included.** Evict by ascending
   segment index. Eviction **may drop a segment that is currently being
   processed** — the worker `load()`s bytes into memory before processing, so
   deleting the file does not corrupt an in-flight upload, and the worker
   already tolerates a vanished file (`NotFound → skip`). The active segment
   is never evicted **except** when it alone exceeds the budget (only
   reachable under misconfiguration), in which case the writer stops and
   deletes it rather than leaking it.

6. **`ENOSPC` is a terminal alarm: wipe and disable.** A real `ENOSPC` (a
   write/seal/rotate failing with `raw_os_error() == ENOSPC`) is treated as
   "something has gone badly wrong," distinct from graceful over-budget
   eviction. On `ENOSPC`, dial9 **deletes all of its own on-disk segment
   families** (every `{stem}.{index}.bin*`, nothing else on the volume) and
   **disables telemetry** for the process lifetime via the existing
   `shared.enabled = false` path. Recording stops; the flush loop and worker
   wind down. Already-started S3 uploads are allowed to finish (they read from
   memory, not the wiped disk); only new work stops being dispensed. It is
   logged once at `error!` (not a suppressed line) and surfaced via the
   `disk_full_encountered` metric. There is no automatic re-enable.

7. **Observability.** New `metrique` metrics:
   - gauge, on `WorkerCycleMetrics`: `retained_bytes`, `retained_segments`,
     `retention_budget_bytes` (so utilization is derivable);
   - counters (accumulated in `DiskFs`, flushed per worker cycle so
     either-thread evictions count): keep `segments_evicted`, add
     `bytes_evicted`;
   - disk-pressure, on `FlushMetrics`: `disk_full_encountered`,
     `write_stopped_no_space`;
   - `reconcile_drift_bytes` — non-zero means something mutated trace files
     outside `DiskFs`; a canary that the exact-model invariant has leaked.

## Alternatives considered

- **Keep eviction in the writer; add a second independent evictor.** Less
  churn, but preserves the two-ledger split-brain that causes today's drift
  and the "eviction stops at `Finished`" bug. Rejected: it keeps the root
  cause alive.
- **Maintained byte counter instead of a reconciling scan.** Faster (no
  per-cycle `stat`s), but every mutation path must report perfectly or the
  counter silently drifts — which is precisely the current failure. Rejected
  in favor of "directory is the truth," with the exact model as a fast cache
  the scan corrects. (Per-cycle `stat` of tens–hundreds of files at ~1 Hz is
  negligible; throttle later only if profiling demands.)
- **Have the writer report live active-segment size to `DiskFs`.** More
  precise total, but adds cross-thread plumbing for the one value `DiskFs`
  can't see, and is inherently racy. Rejected in favor of reserving
  `max_file_size` headroom, which makes the invariant hold without it.
- **Eviction skips in-flight/claimed segments.** Avoids dropping a segment
  mid-upload, but lets the budget flex upward whenever many segments are
  claimed, softening a guarantee we want to be hard. Rejected: the hard cap
  and "lose data rather than disrupt the host" contract win.
- **Recover from `ENOSPC` (inline evict-and-retry, or a `Paused` state that
  retries when space frees).** Maximally durable — survives a transient full
  shared volume without losing the session. Rejected deliberately: a
  disk-full event means something is already wrong; the strongest
  good-citizen stance is to remove our entire footprint and get out of the
  way, with zero risk of dial9 ever being the process holding a full disk.
  The cost (a transient neighbor spike permanently disables dial9 until
  restart) was accepted as the right trade.
- **Bring the memory ring under the same model/metrics.** Uniform, but the
  ring already enforces the contract by construction and has no `ENOSPC`
  analogue. Rejected as needless churn; memory keeps its mechanism.

## Consequences

- Retention survives writer death: cleanup keeps running after `Finished` via
  the worker loop, so dial9 no longer *holds* a full disk after it stops
  writing.
- Accounting reflects reality, including compression, because write-back goes
  through `DiskFs` and a scan reconciles each cycle. `reconcile_drift_bytes`
  makes any remaining leak observable rather than silent.
- `WriteBackProcessor` becomes a `DiskFs` operation — a deliberate API change.
  This also removes the last place that mutated trace files behind `DiskFs`'s
  back, which is what made write-back and eviction race.
- Effective sealed retention is `max_total_size − max_file_size` (with
  defaults, ~3/4 of the budget in sealed segments + up to 1/4 active). This is
  honoring "total ≤ budget," bookkept as a reservation; it is a small,
  intentional reduction in retained sealed data versus a naive reading of the
  budget.
- A hard cap can occasionally drop a segment moments before its upload would
  have completed (eviction and the worker both target oldest-first under
  pressure). Accepted under the good-citizen contract.
- `ENOSPC` permanently disables telemetry for the process. A misconfigured
  budget (greater than free space) or a transient external disk-full will
  wipe dial9's data and turn it off until the process restarts. This is loud
  (an `error!` and `disk_full_encountered`) so the misconfiguration is
  visible, but it is a sharp edge operators must understand: size
  `max_total_size` below the smallest expected free space on the volume.
- Disk-only. `MemFs` is unchanged; its ring already provides an equivalent
  bounded, drop-oldest contract, and the new disk-only gauges are absent on
  memory (mirroring how the `memory_*` gauges are absent on disk).
