# Sampling evidence archive

Proofs, models, reference simulations, review reports and the interactive
visualization are tracked separately from implementation PR #1093.

The current source revision is `aaa8c7208a0d4e6a76608f0d8fc76b4d6bfa42d7`.
Its [automatic population discovery](automatic-reporting.md) means
callers configure a target, while a learned reporting fraction replaces
`minimum_population`. It implements fraction handoffs, quiet-process recovery
and profiling admission independent of census eligibility. Its
[new protocol evidence](automatic/internals/proofs.md) contains the fresh-epoch
hash argument, finite models, simulations and explicit limits.

The archived implementation evidence describes schema-3 independent startup admission
within a configured census subset and Bernoulli thinning, with a scalar current
expansion weight embedded in traces. Its source revision is recorded in
[weighted/README.md](weighted/README.md).

- [Automatic reporting design and implementation checklist](automatic-reporting.md)
- [Automatic reporting proof, model checks and simulation](automatic/internals/proofs.md)
- [Archived schema-3 proof, model checks and validation](weighted/README.md)
- [Archived sequential expansion-weight proof](weighted/statistics/weighting-proof.md)
- [Archived storage/control review](weighted/storage/review.md)
- [Archived fleet laboratory](weighted/visualization/sampling-fleet-laboratory.html)

The files at this directory's root preserve the original evidence for commit
`d719cd52adb020349b119e098552226b124aee90`. Its shared-rank admission and
deterministic rank trimming have been superseded. Those statistical simulations
and keep-cutoff arguments do **not** describe the current implementation or
establish a current exact profiler cap. The original fencing, lifecycle and
clock-boundary models remain historical evidence for their stated abstractions,
rather than a refinement proof of the current Rust code.

## Superseded rank-roster design and validation

Startup reads one compatible roster and compares a stable 64-bit hash rank
with its arm threshold. Missing/unsettled/error cases use the explicit caller
failure policy. Selection freezes before the application builds its instrumented
runtime. The 1000ms default covers runtime/client construction and GET. Zero
target and identities outside the tracked slice perform no coordination I/O.

Tracked active, unarmed and terminal carrier processes maintain empty membership
objects. One elected process lists current/previous time buckets, counts process
incarnations, and publishes cutoffs. Followers PUT their own members and GET the
roster. There are no slot probes, peer GETs or slot-capacity retries.

Lease and roster are one CAS object. Takeover changes the version fencing
publication. Election uses rank-scaled backoff bounded by `min(2s,W/2)`, one re-read,
then at most one CAS
if still expired. Re-reading suppresses most losing writes; simultaneous reads
can still race. Request counts remain proportional to the tracked population.

Arm is derived from the tracked count rather than a target-th order statistic.
Keep can eventually trim excess active processes, never enable unarmed/carrier
processes. Warm leader cadence is `min(1s,W)`, then W after settling.
Target is an expected volume; default reconciliation limit is
`T + ceil(3 sqrt(T))`, not exactly T.

- [Safety arguments and assumptions](proofs.md)
- [Population and recording distributions](estimation.md)
- [Storage, retention and rollout](storage-lifecycle.md)
- [Recorded validation evidence](review-results.md)

## Reproduce

The independent simulator requires Python 3.12+ and only standard libraries:

```sh
python3 docs/design/sampling/fleet_simulation.py
```

It runs 50,000 trials per statistical scenario plus bootstrap, sticky/fresh
cohorts, growth/shrink, overlapping restart, partitions, paused leader, unknown
acknowledgement, cleanup failure, unordered pagination, invalid continuation,
rollover and skew scenarios. Request experiments start 100,000 processes. It is
an independent reference model, not Rust execution or an S3 throughput benchmark.

Run the finite models using Java 17+ and a local TLC jar:

```sh
python3 docs/design/sampling/check_models.py /path/to/tla2tools.jar
```

The runner requires four positive configurations to complete and nine broken
controls to violate their named invariant. Parser errors, missing assignments,
timeouts and unrelated failures fail the check rather than count as counterexamples.

`RosterSampling.tla` checks two leaders with bounded revisions and three process
incarnations in separate configurations. `BoundaryCensus.tla` checks guarded
publication across rollover. `TerminalRegistry.tla` checks that an older
carrier key takes precedence over a delayed active key in a newer bucket.
`OffByOne.tla` demonstrates the discarded strict
T-th-rank rule's identical-cohort boundary error.

## Evidence limits

Models abstract clocks into expiry/safe-phase events, versions into distinct
finite integers and rank startup into a once-only choice. They check safety,
not Rust refinement, fair election, scheduler progress, hashing quality or
census completeness. Statistical simulation shares census and selection rank;
it does not incorrectly treat those variables as independent.

Rust tests separately cover actual asynchronous ordering, API lifecycle,
recorder registration, deadlines, policy isolation and backend validation.
