# Glossary

Project-wide vocabulary. Terms here are the canonical names used in code,
docs, issues, and PRs. If a term you need isn't here, either it's not yet
resolved (raise it) or you're inventing language the project doesn't use
(reconsider).

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
