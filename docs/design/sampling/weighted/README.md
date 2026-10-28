# Weighted fleet sampling evidence

Reviewed implementation: `1904be0b31d3bb49960afe2de4ae9b51f5ab8540`.
Implementation PR: https://github.com/dial9-rs/dial9/pull/1093.
Protocol schema: 3. Trace sampling metadata schema: 1.

The counted unit is a live process/container incarnation that could emit Dial9
traces. Census rank, fresh invocation admission, and fresh salted thinning draws
use separate entropy. Startup selects once; terminally stopped processes never
resume. An expected target replaces any exact profiler-count claim.

## How weighting works

A selected unit starts with factor `1/(f*r)`, where `f` is census eligibility and
`r` is its conditional startup admission rate. Each sealed round it actually
survives multiplies that factor by `1/q` once. Missing rounds add nothing;
new joiners skip previously published rounds. Failed coins write terminal zero
before asynchronous recorder stopping.

The existing roster refresh updates timestamped trace metadata. A decoder needs
one factor for the interval containing each sample, rather than the full roster
or an external join. The full change timeline repeats in rotated segments;
missing/null factors or timestamps beyond an observed checkpoint are unknown.
Independent retained census observations answer the separate visible-population
question and do not substitute for a trace's expansion factor.

[The proof](statistics/weighting-proof.md) establishes sequential conditional
expansion factors under explicit support, independence and delivery assumptions.
It does not assert that the factor is an exact marginal inclusion probability.
A cold Disabled cohort has zero support; choose a positive AssumePopulation
fallback if all startup cohorts must be represented. Missing trace delivery,
manual pauses, churn and census ghosts need separate treatment.

## Reviews and independent models

- [Statistics](statistics/review.md), [weighting proof](statistics/weighting-proof.md),
  and [seeded results](statistics/weighting-results.json).
- [Storage/control](storage/review.md): 108 exact rational admission/thinning
  cases and 18 sparse-census split cases; each has exact population expectation.
  The Python control model checked 627,396 states and 1,731,672 transitions,
  including 504,672 stale-CAS transitions.
- [Schema-3 TLA+ model](storage/tla/README.md): 153,715 distinct states with no
  safety violation. Negative controls detect duplicate application and stale
  CAS acceptance; an additional witness demonstrates allowed missed rounds.
- [Lifecycle](lifecycle/review.md), [permanent stop integration](lifecycle/implementation.md),
  and [trace metadata](lifecycle/trace-weights.md).
- [Protocol/clock review](protocol/review.md).
- [Resolution notes](resolution-notes.md) record fixes to findings in the preserved
  reports, including the actual Worker duplicate/fresh/startup epoch regressions.
- [Validation manifest](validation.json) records final Rust results and limits.

For a heterogeneous fleet workload of 2,080, 60,000 seeded trials estimated
2,068.49 with the sequential factors (Monte Carlo SE 10.37). Scaling by a single
population/profiler ratio estimated only 681.14. This is a model result under its
assumptions, not a production latency or delivery measurement.

## Reproduce

Python checks require only the standard library:

```sh
cd docs/design/sampling/weighted/statistics
python3 checks.py
python3 check_weighting.py
cd ../storage
python3 exact_moments.py
python3 control_model.py
```

Generated JSON records seeds and scenarios. Statistical means are checked against
Monte Carlo error; exact enumeration uses rational arithmetic. Negative controls
reuse admission randomness or repeat a round, demonstrating biased weighting.
Finite control models check their abstractions rather than Rust refinement or
liveness in an arbitrary storage partition. Run the schema-3 TLA+ model and its
expected counterexamples with a local TLC jar:

```sh
cd docs/design/sampling/weighted/storage/tla
python3 run_tlc.py /absolute/path/to/tla2tools.jar
```

## Interactive visualization

Open [the standalone HTML](visualization/sampling-fleet-laboratory.html), or the
uploaded artifact in an authenticated browser:
https://artifactory.amazon.dev/view/sampling-a-changing-fleet-census.

It supports cold start, rolling start, one host starts per minute, and lots of
containers. It shows ground-truth live processes, projected census population,
actual profiling units, and expanded trace totals. A lifespan control also
allows one-minute-lived units.

Gaussian census references describe independent hash error only; sparse and
full-census cases are handled explicitly. Profiler counts remain discrete, and
the empirical weighted-error histogram includes tails with a descriptive fitted
Gaussian. Neither curve is a universal error guarantee. Ghosts, missing members,
staleness, zero support and ratio-estimator error are shown separately.

```sh
cd docs/design/sampling/weighted/visualization
python3 build.py
node check.js
node check-browser.js
```

The browser check uses locally installed Playwright/Chromium and forbids network
requests. These resources and dependencies are not part of the implementation PR.

The current viewer does not yet apply these trace factors. The sampler now emits
the input needed by a weighted flamegraph reader.
