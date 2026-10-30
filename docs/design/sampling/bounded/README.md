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
  Normal application exit stops new observations and control work, then drains
  already captured immutable reports within one shared 9-second cleanup budget.
  This reference rule is pending final Rust integration; it does not guarantee
  delivery after crashes, backend failure, or a late server commit.
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

[Exact workload lineage](weight-lineage-results.json) independently enumerates
72 rational-probability branches for four processes with unequal startup
probabilities, two adaptive trims, source-cohort exemptions, differing lifetimes,
and changing CPU work. At every one of five time slices, each live process's
expected weighted work equals its actual work. At 200 seconds, workload truth
and correct expansion are both 21; omitting the first `1/q` gives `56/3`,
applying it twice gives `224/9`, and replacing the weights with an exactly
correct current population divided by profiler count gives `185/12`.
These are expectation identities, not a claim that one small sampled
flamegraph closely matches its fleet.

[Report lifetime checks](report-lifetime-results.json) expose delivery bias
separately from sampling variance. For a 60-second lifetime and healthy
2.5-second operations, immediate exit cancellation loses the last `5/60` of
census observations and `2.5/60` of profiler observations. Draining captured
jobs for at most nine seconds removes that loss in the explicit successful
paths, including one failed PUT followed by a successful retry. The exact
integration is checked by 200,000 seeded opportunities with independent rank
eligibility. A half-window lifetime still contributes one half; drain neither
creates a catch-up opportunity nor counts the process as alive after exit.
The crash and failed-delivery controls retain their bias. A Gaussian error
band cannot describe this systematic censorship.

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

## Simulation invariants and remaining checks

The cohort simulator asserts integer membership, report opportunities within
lifetimes, recording support, positive finite gates/weights, conserved members
on cache updates, once-only source reservation, and source-aged publication
on every step. Its stochastic accuracy results do not replace structural tests.
The most useful adversarial invariants distinguish illegal behavior from
legitimate sampling error:

| Invariant | Current independent evidence | Mutation or next probe |
| --- | --- | --- |
| Namespace construction cannot place another channel, pool, or caller dimension inside a source-window prefix. | Direct Rust namespace regression; the models treat keys as abstract identities. | Give a caller dimension the reserved name `window`; assert prefix isolation through escaped sorted keys. |
| Known incompatible wire schema prevents participation even if its body cannot deserialize as the current format. | Direct Rust previous/future-schema regressions; no wire-format model proof. | Treat schema 4 as transport failure and incorrectly enter the uncoordinated fallback. |
| A report describes presence and recording at its captured instant; normal exit cannot erase a bounded successful captured job. | Exact and seeded report-lifetime reference. | Cancel immediately, or re-evaluate recording after shutdown; keep crash/failure cases separate. |
| Actual `1/p × product(1/q)` conserves each supported process's exogenous work in expectation. | Exact adaptive lineage oracle, including source-born exemptions. | Missing or duplicate `1/q`; substitute population/profiler-count; use unequal CPU and lifetimes to expose cancellation. |
| Current rank, cutoff, and observation instant remain frozen despite roster refresh, elections, retry, birth, or resume. | Finite reroll control and lifetime reference; cohort step assertions. | Trace per-run opportunity/cutoff through delayed cache arrivals and suspension without catch-up. |
| Reservation, result publication, and population source age remain distinct through takeover and uncertainty. | Finite fenced-CAS/reservation controls; cohort step assertions. | Replay an uncertain reservation followed by takeover; reject a second LIST or freshened old observation. |
| Every source obeys the LIST/key cap; incomplete or invalid prefixes never become a numerical population. | Finite cap/short-page controls and cohort step assertions. | Replay sorted short pages, repeated cursors/identities, malformed keys and exhausted native retry attempts. |
| Quiet scheduled opportunities remain at most 300 seconds apart under healthy timing. | Exact nested-phase checks and fair finite quiet-progress model. | Independent phase redraw or active/quiet phases; keep GET failures and scheduler suspension outside the healthy bound. |

Direct Rust/backend correspondence, exact request ledgers under uncertain
remote completion, and long suspension still need review against the final
implementation revision. A default-disabled cohort has no profiling support;
conservation must report that limitation rather than invent a weight.

## Reproduce

Python 3.11+ and NumPy are evidence-tool dependencies only; Java 17+ and the
local `tla2tools.jar` run TLC:

```sh
python3 docs/design/sampling/bounded/check_statistics.py
python3 docs/design/sampling/bounded/check_weight_lineage.py
python3 docs/design/sampling/bounded/check_report_lifetime.py
python3 docs/design/sampling/bounded/simulate.py --trials 40 --comparison
python3 docs/design/sampling/bounded/check_models.py /path/to/tla2tools.jar
```

Results are seeded independent reference experiments. Actual async timing,
consumer metadata, cancellation, and storage behavior require separate Rust
tests and review against the final implementation revision.
