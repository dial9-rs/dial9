# ADR-0002: Block-in-place gap is unknowable; samples in the gap are dropped from worker views

- **Status:** accepted
- **Date:** 2026-05-19

## Context

When `tokio::task::block_in_place` runs on a worker `W`'s OS thread `A`,
tokio races a blocking-pool thread `B` to take over `W`'s core. The
trace has no event marking the moment of handoff. The first authoritative
signal that the binding has moved is a `WorkerPark { worker_id: W, tid: B }`
event some time later. The interval `[last_event_on_W_at_A, park_at_B)`
contains the handoff at an unknown instant.

During this interval:
- Samples on tid `A` could be running the user's blocking closure (no
  longer a worker) or, briefly, still scheduling W (if the handoff
  hasn't happened yet).
- Samples on tid `B` could be running W's scheduler (after handoff) or
  doing unrelated blocking-pool work (before handoff).

The trace data alone cannot determine which is which.

A second issue: the active-span ratio computed by `buildWorkerSpans`
divides `cpuDelta` (a CPU-time delta read on the parking thread) by
`wallDelta`. When the unpark and the park happen on different tids, the
two `CLOCK_THREAD_CPUTIME_ID` readings are from different threads and
are not differenceable; the resulting ratio is meaningless.

## Decision

Treat the `[last_event_on_W_at_A, park_at_B)` interval as a
**block-in-place gap**: an interval whose worker attribution is
unknowable. The analysis layer:

1. Detects gaps by streaming park/unpark events in timestamp order:
   any park or unpark on worker `W` whose tid differs from the
   currently-bound tid for `W` opens a gap.
2. Records each gap as a top-level `trace.blockInPlaceGaps` entry
   `{workerId, fromTid, toTid, startNs, endNs}`.
3. Rewrites `cpuSample.workerId` to the off-worker sentinel (`255`) for
   any sample whose timestamp falls inside a gap, regardless of its tid.
4. **Discards** any active-span (`buildWorkerSpans`-computed) that would
   cross a gap boundary. We do not split it; we drop it. The CPU-time
   delta is meaningless and any partial reconstruction would be an
   invitation to misinterpret.
5. Renders gaps as a distinct overlay on the affected worker's timeline.

Detailed per-tid investigation is left to the agent toolkit, which
exposes `trace.blockInPlaceGaps` and provides a recipe for inspecting
stacks on `fromTid` and `toTid` separately during the gap window.

## Alternatives considered

- **Backfill attribution.** Choose a heuristic start point (last event
  on the old tid, last sample on the old tid) and confidently attribute
  samples on each tid to either "blocking" or "worker" within the gap.
  Rejected: any choice of start point is wrong some of the time, and
  the user has no way to tell when. The viewer would silently report
  incorrect data.
- **Strict (no gaps, no annotation).** Treat every park/unpark as
  authoritative for its own moment, leave `worker_id` wrong elsewhere.
  Rejected: this is what the bug is.
- **Producer-side `block_in_place` event.** Out of scope (see
  ADR-0001). Even with such an event, the trade-off about polluted
  active-spans across the gap remains.

## Consequences

- The viewer is honest about uncertainty. Users see "block_in_place
  happened here" rather than fabricated worker attribution.
- We lose worker-level data inside gaps. Per-task allocation totals,
  per-worker flamegraphs, and active-span ratios all drop the gap
  window. A user trying to investigate the gap must use the toolkit
  to query by tid.
- The active-span suppression is a small UX regression for traces
  with frequent `block_in_place`: those workers will show fewer active
  spans. This is intentional — the prior numbers were wrong.
- Open intervals at trace boundaries cannot trigger gap detection.
  A `block_in_place` that begins before the trace started, or ends
  after the trace ended, is invisible to this analysis. Documented as
  a known limitation.
