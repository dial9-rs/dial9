# Bounded rotating-census evidence

This directory is tracked on the separate evidence branch. No model, simulator,
result file, or new dependency belongs in implementation PR #1093. The previous
selected-only and reporting-epoch resources remain unchanged and historical.

Implementation base: `22cc577986b530fd05b27eac8c2751130c533aca`.
The initial wire/controller milestone reviewed here is `59806840`; the final
implementation source is explicitly pending in `validation.json`. These
resources validate the stated model contract, not arbitrary later code.

## Contract exercised

- Completed 60-second windows estimate mean concurrent **live sampling
  participants / potential trace emitters**, including quiet or disabled
  processes and containers, not physical machines or instantaneous
  population. Fresh independent ranks/instants and recorded actual cutoffs
  support the estimator `sum 1/min(c_i,theta)`.
- One fixed uniform 300-second roster phase produces quiet polls; recording
  processes use its nested 60-second subslots. Every process may contest an
  authoritatively expired lease. GETs only change future census lotteries.
- A verified complete sample with fewer than `K/2` retained reporters raises
  future cutoff to `max(min(1,2*c_old), K/N_hat when positive)`. Empty means
  unknown. Unusable LISTs hold the cutoff. Valid admission ages from the source
  window end for 600 seconds, independently of reservation/renewal CAS.
- Reservation CAS precedes LIST; result CAS is separately fenced. Each source
  window is reserved once even through takeover or uncertain publication.
  At target 15: **2 logical LISTs / at most 546 keys per source**, nominal
  **496 GETs + 497 PUTs/minute plus `N/5` scheduled GET opportunities**.
  The GET sum conservatively counts recording processes' quiet slots again;
  nested slots and due-work coalescing reduce actual calls. Startup, elections,
  request retries, and transitions are separate.
- Reporting requests have 3-second operation deadlines, at most two identical
  PUT attempts, 2-second dispatch grace, end+11-second submission deadline,
  and end+22-second closure under the default 5-second skew setting.
- Profiler PUT instants remain uniform and independent of fixed control GETs.
  A trim reserves the first UTC boundary strictly after the pre-publication
  time+81s: 75s for propagation (`60s active cadence + 2*5s skew + 3s GET
  + 2s dispatch`) plus 6s for bounded publication/readback. Its source cohort
  is `born_window <= source_window`; a new trim uses source at least
  `previous_effective+1`. A missed token is skipped, never applied late.
  Surviving scalar trace weight remains `1/p * product(1/q)`.

## Measurements and qualifications

[Simulation results](simulation-results.json) use seed 1093 and 40 trials per
case. A cohort represents equal cache/lifetime/phase/recording/weight/workload;
there is no Python object per member of a million-process fleet. Fixed
15-second phase grid points approximate the uniform roster phase. Census gates
are quantized to four significant digits and that **actual** gate is recorded.
The model assumes ideal request timing and complete dispatch/delivery except
explicit loss/outage cases. It is not a network throughput or Rust simulation.

The active 60-second control cadence materially reduces transient profiling:

| Scenario | Conservative 313s lead / six-window cooldown | Active 75s propagation + 6s guard / one-window cooldown |
| --- | ---: | ---: |
| Cold 100,000, fallback probability 1 | 700,344 profiler-minutes | 300,373 |
| Warm 1,000 → 100,000 growth | 11,295 profiler-minutes | 5,389 |
| +1,000 starts/minute for 60 minutes | 651 excess profiler-minutes above 27 | 143 |

The comparison uses identical seed streams, not identical paired process
identities after diverging control paths. Cold fallback probability 1 means
`StartupFailure::assume_population(15)` at target 15; it introduces no new
public fallback mode. Default-disabled cold processes remain unprofiled.
Even the faster trim leaves a three-minute all-profiled cold transient in this
probability-1 scenario. There is no exact or promptly restored profiler quota.

The final shown window gives these trial means. Source truth is the older
completed window being estimated; it explains the rolling fleet's lag.

| Fleet | Current mean processes | Source mean processes | Projected processes | Actual profilers |
| --- | ---: | ---: | ---: | ---: |
| Warm | 100,000 | 100,000 | 98,732 | 14.35 |
| Rolling starts | 60,000 | 59,000 | 58,793 | 21.08 |
| One-minute processes | 10,000 | 10,000 | 9,902 | 14.80 |
| Lots of containers | 500,000 | 500,000 | 502,737 | 14.03 |

Million-to-1,000 shrink first produced a population estimate within 10% at
median nine minutes / 95th percentile ten minutes in the current run.
Million-to-one discovery occurred by the next scheduled read (maximum 292.5s
on this grid), while its first exact estimate was around 16–19 minutes.
“First within 10%” is not a bound or a sustained-convergence claim; full
percentile snapshots distinguish transient error from settled behavior.
Both shrink cases had zero surviving profilers in the final snapshots of all
40 trials; census recovery does not restore the departed profiling cohort.

LIST-only outages hold the cutoff near its pre-outage value; source-aged
population becomes unknown rather than fictitiously fresh. A complete outage
doubles failed report PUT attempts through the explicit retry cap. New
processes whose startup GET fails use the disabled fallback and local cutoff
1; their initial reporting burst is separate from the settled fleet budget.
These cohorts stay unsupported for profiling after recovery; scalar weights
cannot reconstruct their absent workload. Unobserved
30% report loss biases projected population toward 70% of truth, even though
the actual admission/thinning probabilities still define valid trace weights.
Unobserved *trace* loss separately biases weighted analysis.

[Exact/statistical checks](statistical-results.json) run 10,000 settled trials:
population ratio mean 1.00029, SD 4.52%, empirical 95% interval
0.9146–1.0926. A Gaussian ±9% reference applies to that settled setting only.
Profiler variation is separate (startup SD approximately `sqrt(15)`).
The weighting experiment with a 30%-low census and `q=0.5` expands to about
0.993× population / 0.996× workload truth; omitting `1/q` gives about 0.496×.
It tests probabilities actually used, not substitution of a census ratio.

[Proof](proof.md) distinguishes the exact continuous-rank theorem from the
implemented integer-rank zero-boundary guard. Skew shifts the real observation
interval; missed dispatch/delivery, disabled support, and within-segment
metadata changes remain explicit qualifications.

## Finite model evidence

[TLC results](model-results.json): `BoundedCensus.tla` explores **613,918 distinct
safe states**, with two processes, two source windows, CAS versions 0–4 and
one trim. All eight negative controls violate their named invariant:
current-window reroll, uncapped LIST, false short-page exhaustion, double trim,
unfenced result/election CAS, reservation freshening population, and re-enable.

`QuietProgress.tla` separately checks progress for one live quiet process under
fair healthy GET/election completion and timely deadline service, with no
contender. Reporter-only reads and repeated phase redraws violate its progress
properties. This proves only the explicitly assumed finite healthy path, not
production election or network liveness.

## Remaining adversarial checks

The cohort simulator asserts integer membership, report opportunities within
lifetimes, recording support, positive finite gates/weights, conserved members
on cache updates, once-only source reservation, and source-aged publication
on every step. Its stochastic accuracy results do not replace structural tests.
Further checks should track each selected run's probability lineage and CPU
work under correlated churn (mutations: missing or doubled `1/q`); freeze the
cutoff and opportunity through roster arrival and process suspension
(mutations: current-window reroll or catch-up); and replay takeover,
short-page/retry request ledgers and uncertain CAS without refreshing source
age. The finite models cover abstractions of those protocol rules; direct
Rust/backend correspondence still needs the final implementation revision.

## Reproduce

Python 3.11+ and NumPy are evidence-tool dependencies only; Java 17+ and the
local `tla2tools.jar` run TLC:

```sh
python3 docs/design/sampling/bounded/check_statistics.py
python3 docs/design/sampling/bounded/simulate.py --trials 40 --comparison
python3 docs/design/sampling/bounded/check_models.py /path/to/tla2tools.jar
```

Results are seeded independent reference experiments. Actual async timing,
consumer metadata, cancellation, and storage behavior require separate Rust
tests and review against the final implementation revision.
