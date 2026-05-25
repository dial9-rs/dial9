# ADR-0001: CpuSample.worker_id is derived at analysis, not authoritative on the wire

- **Status:** accepted
- **Date:** 2026-05-19

## Context

`CpuSample` events carry a `worker_id` field on the wire. The producer
fills it from `SharedState.thread_roles`, a `tid → ThreadRole` map
maintained by the runtime: entries are inserted on `on_thread_start` and
removed on `on_thread_stop`.

`tokio::task::block_in_place` does not fire either of those callbacks (see
research in conversation; tokio's `block_in_place` hands the worker's
core to a replacement thread without going through `Context::park()`).
The original OS thread keeps running, but stops being a worker; the
producer-side `thread_roles` map continues to claim it is. Every CPU
sample taken on that thread during the `block_in_place` window therefore
carries an incorrect `worker_id`.

The on-wire field cannot be made correct from the producer side without
a tokio API surface we don't subscribe to, and even then there are races
(the role flips at instants the producer can't observe synchronously).

The trace already carries `WorkerPark.tid` and `WorkerUnpark.tid`. Those
events bracket the intervals during which a given OS thread is acting as
a given worker. They are the authoritative signal.

## Decision

Treat `CpuSample.worker_id` on the wire as a deprecated hint. The JS
analysis layer derives the correct attribution post-decode by streaming
park/unpark events in timestamp order and rewriting each sample's
`workerId` field in place. The on-wire field will be removed from the
trace format in a future change.

We do not change the producer. We do not introduce a new "derived
worker_id" field; the existing field name keeps its meaning ("the worker
this sample belongs to"), but the source of truth shifts from the wire
to the analysis pass.

## Alternatives considered

- **Hook tokio's `block_in_place` from the producer.** No public hook
  exists. Patching tokio is out of scope. Even with a hook, the role
  flip is racy with sample collection.
- **Emit `worker_id = UNKNOWN` from the producer when uncertain.** The
  producer cannot determine it is uncertain — the `thread_roles` map
  doesn't know about `block_in_place`. We'd have to plumb a separate
  signal in, equivalent to the analysis-side fix in complexity.
- **Add a separate "derived_worker_id" field on the parsed sample,
  preserving the wire field.** Rejected as needless plumbing for
  callers; the wire field has no remaining use case once the derivation
  exists.

## Consequences

- Tooling that reads `CpuSample.worker_id` from a parsed trace (Rust
  `analysis.rs::TraceReader`, JS `trace_parser.js`) gets the correct
  value transparently. The Rust side is left unchanged in this round
  (tracked separately); the JS side fixes both the parsed value and
  downstream consumers.
- Tooling that reads the wire field directly (e.g. external consumers
  of the binary format) keeps the broken value until the field is
  removed.
- Removing the wire field is a future trace-format change; old traces
  will have it and the JS decoder must continue to ignore it gracefully.
