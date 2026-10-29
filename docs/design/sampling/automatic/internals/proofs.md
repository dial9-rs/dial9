# Automatic reporting protocol: claims and limits

The implementation is schema 4, with service-isolated store namespaces and
`v3` roster, membership and observation keys. The Rust tests exercise the
implementation; the finite model and simulator are independent abstractions.
They do not prove correctness of arbitrary storage implementations or an
instantaneous liveness estimate.

## Fractions and census epochs

An active reporting epoch owns both a fixed integer threshold and a membership
namespace. There is at most one pending epoch. Its threshold is chosen from the
previous observation before generating its fresh epoch ID, which domain-separates
the census hash. The epoch ID is reused for all reports in that epoch.

Under independent uniform hash outputs, a fixed live population independent
of these hashes, complete membership delivery, and a fixed observation time:

```text
R | preceding history ~ Binomial(N, f)
E[R/f | preceding history] = N
Var(R/f | preceding history) = N*(1-f)/f
```

Fresh epoch entropy matters. Retaining the same ranks while choosing `f_next`
from their previous count creates feedback. For an interior adjustment
`f_next = B*f_old/R_old`, conditioning on `R_old=r` gives:

```text
E[N_hat_next | r]
  = N + (r-N*f_old)*(1/f_next-1)/(1-f_old).
```

Substituting the adjustment and averaging the unbounded interior algebra gives
relative bias `1/B`. Clipping, zero counts, hysteresis, fleet churn and selection
of observation times change this calculation. At `B=480`, the simulation control
measured about +0.23% bias; fresh epoch ranks measured about +0.016%, consistent
with Monte Carlo error. Both had about 3.3% standard deviation in the tested
10,000-to-1,000 shrink experiment. This is not a universal confidence bound.

The production controller only changes fractions after three complete counts
outside `[B/2, 2*B]` and plateau settling. Zero sampled reporters immediately
schedule a full-reporting handoff. Historical observations record the actual
epoch and integer threshold, and count only its census-eligible records.
Profiler records outside that threshold do not enter `R`.

## Handoff and recovery

Old reporters continue the active epoch. Profilers report in both active and
pending epochs, but each census deduplicates only its own namespace. Startup
continues using the prior usable population observation, with freshness bounded
by `2*bucket + 6*poll + 2*skew`. Zero reporter counts never overwrite it with zero.

Collection waits `5*poll + 2*skew + backend timestamp uncertainty`; promotion also
waits for four complete, guarded pending censuses satisfying the plateau
heuristic. The collection delay covers configured quiet polling and bounded
operations under healthy delivery. It cannot certify that partitioned or
incorrectly delayed processes reported. Pending failure does not reinterpret old
membership under a new fraction. Takeover retains the pending epoch and deadline.
If the active epoch loses every reporter while a non-full pending epoch is
warming, that pending epoch is abandoned in favor of full reporting recovery.
Any census already read from the abandoned epoch is fenced from promotion by
its epoch ID. A pending full-reporting recovery keeps its original deadline.

The leader and reporting policy share a CAS object. Every mutation has a fresh
body revision. A stale conditional publication cannot replace a newer owner;
an uncertain write outcome requires readback rather than a blind retry.
Compatible quiet processes may take over after lease expiry. Losing every
previous reporter does not require a surviving reporter to begin full reporting.
This establishes a recovery path under healthy delivery and fair polling, not
an unconditional recovery-time bound during partitions.

## Profiling and weights

The startup draw is independent of reporting eligibility. Its actual integer
probability is `p=min(1,T/N_hat)` or the explicit failure probability. For any
positive probability selected before the fresh admission coin:

```text
E[admitted * workload/p | preceding history] = workload.
```

A thinning round seals `q` before its independent fresh round coins. Conditional
on the previous selection history, the same identity holds after multiplying a
survivor's weight by `1/q`. A process applies an observed round at most once.
Missed rounds are not retroactively weighted; new startups skip already-sealed
rounds. Thus the stored weight reflects the probabilities actually applied.
Population estimation error changes volume and variance without itself
invalidating these conditional inverse-probability weights.

Changing reporting epochs neither reveals another admission coin nor changes
an existing scalar trace weight. Sampler choices move only from active to
terminal; application-controlled `enable()` remains an explicit override outside
the sampler's assumptions. A zero startup probability has no profiling support,
so neither historical population evidence nor later traces can reconstruct that
cohort. Metadata remains a scalar snapshot at evaluation time, with the accepted
approximation that it applies to the segment.

## Executed evidence

The model runner needs Java and a local TLC jar. The Python simulator needs
NumPy; the validation manifest records the Python and NumPy versions used.
Run from this directory:

```sh
python3 check_models.py /path/to/tla2tools.jar
OPENBLAS_NUM_THREADS=1 python3 simulate.py
```

The finite two-process model checks epoch/fraction pairing, collection readiness,
quiet recovery eligibility, stale CAS fencing, once-only weighting and terminal
startup decisions. Seven deliberately broken controls violate the corresponding
invariants. It abstracts pagination completeness and bounded clock arithmetic;
paused-time Rust tests cover those implementation details. The model uses a
two-step collection clock and does not establish statistical error bounds.

The simulator records actual live process/container counts, projected population,
actual profilers, weighted units, fraction changes and GET/PUT/LIST/CAS/DELETE
counts for cold and rolling startup, one start per minute, one-minute lifetimes,
many containers, growth/shrink, loss of all reporters (including during a
pending reduction), delayed handoff reads,
failed writes/cleanup, and a partition. Ten-second ticks conservatively discretize
the fast bootstrap census cadence. Lease contention and stale publishers are
tested in Rust/TLA rather than modeled as network packets in this simulator.

Delayed reads and failed cleanup are explicit counterexamples to treating the
Gaussian reference as a total census error bound. Ghosts remain visible for up
to two buckets. A cold default-disabled fleet may learn its population while
producing zero profiles forever if no new process starts. A shrinking fleet may
lose its profilers; discovery recovery does not turn previously off processes on.
