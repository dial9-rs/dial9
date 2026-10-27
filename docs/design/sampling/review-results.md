# Startup sampling review and validation

The sampler is independent of recording and owns a private coordination runtime.
Applications choose profiling before creating the instrumented runtime, retain
the session with `Recorder::attach_object`, and wire the optional terminal-stop
future. Default continuity and fixed-probability fallback can exceed the target.

## Correctness checks

- Conditional S3 writes use one SDK attempt. A lost committed response remains
  an unknown outcome for owner-checked reconciliation, instead of becoming a
  precondition failure hidden behind an SDK retry.
- Definitive conflicts retry the same slot within the startup budget. Unknown
  writes fence that slot and cannot allocate another one.
- Reuse includes backend modification-time uncertainty. Native S3 declares one
  second, delaying reuse even with zero additional clock skew.
- Maintenance panic/abort invalidates coordination and internal estimates. The
  strict stop future completes on loss or monitor closure. It never requests a
  later resume. Default continuity preserves the original profiling choice.
- Cleanup checks the known slot's owner; uncertain admissions use a bounded scan.
- Membership scans retain completed-page progress, heartbeat previous members
  before scanning and invalidate estimates after failed/invalid refreshes.
- Startup timeout includes runtime/backend construction and admission. Exhausted
  budgets apply fallback before polling the backend or entering admission.
- Session drop retains its executor through bounded cleanup. Dropping inside
  async code or on the sampler's own worker does not block that executor.
  Canceling explicit shutdown does not cancel release.

## Test scope

Paused-time tests exercise deadline composition, fallback, partitions, unknown
writes, stale membership, clock precision, policy refresh and storage lifecycle.
Real-runtime tests use channels and gated storage to check initialization panic
and cancellation, caller-executor teardown, own-worker drop and release. Recorder
tests check object destruction after finalization and graceful worker drain.
Manual integration uses separate SDK clients against one local S3 server and
decodes an uploaded trace marker; denied admission allocates no trace writer.

The retained heavy qualifications cover three 100,000-process fleet layouts,
512 estimator trials and a bounded-capacity cold start. They are ignored in the
ordinary suite. Run explicitly:

```sh
cargo test --release -p dial9-sampling --lib qualification \
  -- --ignored --nocapture --test-threads=1
```

Shuttle exercises concurrent claims, late writes/cleanup, unknown acknowledgements
and recorder finalization races. It samples schedules; it does not exhaust every
ordering of 100,000 real threads. Run the separate suites:

```sh
./scripts/test-shuttle.sh
```

The bounded TLA+ continuity and strict models check frozen choices, terminal
stops and conditional slot ownership. Continuity's actual-profiler-cap variant
intentionally has a counterexample. The model is not a refinement proof of Rust
and assumes timely local stop enforcement for strict actual-cap checks.

## Limits

The simulations do not qualify production S3 throughput or an arbitrary fleet's
startup latency. Virtual lifecycle tests do not establish a production retention
soak. Estimator bounds require stable, complete participation and the bottom-k
rank assumptions in [estimation.md](estimation.md). Unknown small pools, churn
and partitions have no universal total-fleet bound.

Line coverage is not branch coverage, and the suite does not claim every possible
backend, OS/runtime or scheduling failure is covered. Explicit error regressions
and ownership/race checks supplement statistical and bounded-model evidence.
