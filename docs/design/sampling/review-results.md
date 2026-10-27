# Roster validation evidence — October 27, 2026

The slot allocator and bottom-k estimator have been replaced. This evidence
checks the roster/elected-leader protocol; old slot-model results do not apply.

## Finite models

All thirteen checks completed with their expected outcome using Java 17 and TLC
2026.10.23.135453. Four positive configurations completed without invariant
violations; nine deliberately broken controls violated their named invariant.

| Configuration | Distinct states | Expected result |
| --- | ---: | --- |
| leader | 3194 | No violation |
| lifecycle | 4096 | No violation |
| boundary | 144 | No violation |
| terminal-registry | 5 | No violation |
| negative-split-objects | 456 | FencedPublication |
| negative-incomplete-list | 34 | CompletePassOnly |
| negative-reused-key | 111 | IncarnationIsolation |
| negative-late-enable | 8 | NoLateEnable |
| negative-terminal-trim | 43 | TerminalTrim |
| negative-off-by-one | Initial state | DesiredCount |
| negative-boundary | 14 | GuardedCensus |
| negative-empty-arm | 31 | NonemptyArm |
| negative-bucket-first | 5 | TerminalClassification |

The full output, state totals and expected violations are in
`model-results.json`. Positive checks cover combined-object CAS fencing with
unknown acknowledgements, frozen startup, terminal carrier transitions,
incarnation cleanup, complete-pass gating and guarded start/finish/rollover.
They are finite safety checks, not unbounded Rust refinement or liveness proofs.

## Seeded distribution experiments

Seed 20261027; N=100,000; 50,000 trials per target/tracking policy.
Below are independent next-cohort results before terminal trimming. The JSON
also contains same-census-cohort distributions, histograms and exact references.

| Target | Tracking fraction | Mean count | Count SD | Empirical central 95% | Above default reconciliation limit |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 15 | 1 | 15.006 | 3.870 | 8–23 | 0.178% |
| 15 | 0.0078125 | 15.017 | 3.906 | 8–23 | 0.190% |
| 500 | 1 | 499.973 | 22.278 | 457–545 | 0.128% |
| 500 | 0.25 | 499.997 | 22.408 | 457–544 | 0.120% |

For target 15 and f=1/128, the population estimate had relative SD 3.561%,
central 95% error -6.816% to +7.136%, and 99.514% of trials were within ±10%.
For target 500 and f=1/4, population relative SD was 0.545% and central 95%
error -1.072% to +1.076%. Full tracking has no hash-census noise in this
complete-membership model. Empirical percentages carry Monte Carlo error.

## Adversarial behavior

The discrete-event model passed these assertions:

- Cold Disabled remained off after census settlement; AssumePopulation recorded.
- Stable restart selected the same identities; delayed old cleanup left fresh keys.
- Older-bucket carrier state defeated a delayed newer-bucket active key for the
  same incarnation. Short polling intervals bounded both election and warm cadence.
- Overlapping restarts counted both process incarnations, not one logical host.
- A paused old leader was rejected after takeover; committed unknown claim reconciled.
- Takeover cleared stale keep; a fresh server lease could not authorize trim from
  a census older than its permitted age. Disabling that age check stopped an
  entire new cohort in the corresponding negative control.
- Failed/incomplete/repeated-token LISTs did not publish census.
- A partition allowed 58 active against limit 27; recovery trimmed to 27 active
  processes, with 31 terminal carriers. This demonstrates why the cap is conditional.
- Crash members remained during bucket visibility and dropped after rollover.
- Guarded future-bucket startup stayed on explicit fallback: 21 active after another
  1,000 starts. Disabling guard/zero-count protection reproduced 1,017 active.
- A LIST crossing a bucket did not establish a usable arm; failed cleanup left
  isolated stale memberships.

Growth and shrink scenarios also preserved frozen choices; shrink could not
backfill missing profilers. All outputs are in `simulation-results.json`.

## Storage work and practical recommendation

One synchronized 100,000-process cold start selected 16 processes, an acceptable
outcome for target 15. The first twenty seconds had these operation counts:

| Operation | Full tracking, R=100,000 | f=1/128, R=791 |
| --- | ---: | ---: |
| startup_get | 100,000 | 791 |
| member_put | 100,000 | 791 |
| background_get | 100,000 | 791 |
| election_recheck_get | 100,000 | 791 |
| election_cas | 50 | 2 |
| failed_election_cas | 49 | 1 |
| list_page | 404 | 8 |
| publication_cas | 5 | 5 |

Slot probes were zero. Approximate steady operation rates are 120,201/minute
for full tracking and 952/minute for this sparse sample. Election uses one
re-read after backoff and an illustrative 1ms GET-to-CAS gap. Other operations
are instantaneous and unbounded concurrency is assumed: these are request
counts, not measured S3 capacity or latency. Adversarial concurrency can still
produce one losing election CAS per tracked contender; no constant-CAS bound
is claimed. Background election reads are separate from the one startup GET.

Use the count-derived arm and accept ordinary count variation. A target of 15
has an exact complete-census 95% reference interval of 8–23; 16 is harmless.
Use optional sparse tracking only with a credible per-pool lower population
bound, since it greatly reduces storage work while adding little count noise.
Use an explicit population fallback for required cold-pool coverage. Monitor
count/cutoff timestamps, slice warnings, leader errors and carrier counts;
a plateau does not certify participation. Outages and reused stale cutoffs
remain outside universal count bounds.

Rust lifecycle, storage adapter, deadline, clock and concurrency tests are
additional implementation evidence; this independent model does not execute
Rust or qualify production throughput. See [proofs.md](proofs.md) for assumptions.
