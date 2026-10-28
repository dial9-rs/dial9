# Statistical and retrospective sampling review

Reviewed the original roster implementation at 95b4639c, the retained-census
addition, and the proposed independent admission/sequential thinning design.
No production files were edited by this review.

## Findings and decisions

1. **Population scaling is insufficient for fleet flamegraphs.**
   `N_hat / number_of_profilers` assigns the same weight to heterogeneous
   startup cohorts, whose inclusion rates can differ substantially. It also
   ignores terminal trimming. A toy fleet with true workload 2,080 produced
   ratio-scaled workloads around 617–681, while sequential expansion weights
   recovered approximately 2,065–2,068 with Monte Carlo uncertainty of 10–13.
   Record the actual conditional factors and use the weight in force at sample
   collection time.

2. **Independently randomized admission and fresh trim rounds give a short
   proof.** Keep the census rank for tracking. Use independent per-incarnation
   admission entropy and per-round trim entropy; apply each round at most once.
   Update each survivor's expansion weight by `1/q`. This gives a sequential
   martingale proof even with asynchronous application, missed rounds, stale
   counts, and q depending on prior selections. The product of conditional
   rates is not necessarily the emitter's unconditional marginal inclusion
   probability; call its inverse an expansion weight. See
   [weighting-proof.md](weighting-proof.md).

   Final read-only Rust review confirmed independent invocation admission,
   tagged fresh round entropy, schema 3, mutate-before-compute epochs,
   idempotent round application, no-session fallback weights, full source-clock
   history/checkpoints, and synchronous zero-weight exclusion before a failed
   trim's asynchronous recorder stop. The encoded-segment regression exercises
   frames with weights 10, 20, and terminal zero. The previously identified
   stop-monitor lag bias is addressed.

3. **Zero-support cohorts are fundamentally unidentifiable from traces.**
   A permanently off cold `Disabled` cohort has no stack information.
   Positive `AssumePopulation` fallback supplies coverage even when its
   population assumption is inaccurate; the realized rate provides the
   expansion factor. No finite weighting repairs deliberately unsupported
   cohorts, unknown recorder pauses, failed uploads, or buffer losses.

4. **A retained complete census is useful evidence, not instantaneous truth.**
   Save every successful guarded enumeration independently of whether roster
   CAS succeeds. Retain start/completion times, bucket widths, clock margins,
   tracking fraction and incarnation counts. A time query should return the
   preceding observation with explicit age and `None` for absent/stale
   evidence. The reviewed `history.rs` implements these semantics.
   Two censuses at 0s and 60s, each observing 100 members, cannot distinguish
   a stable population of 100 from 1,000 additional processes that started at
   30s and exited gracefully at 50s. At 40s the actual totals differ by 11×.

5. **Membership bias is separate from Gaussian noise.**
   With 300s buckets and failed cleanup after 60s lifetimes, the two listed
   buckets can show 5–10× as many membership objects as live emitters.
   Missing registrations, incomplete temporal coverage and clock violations
   are systematic/coverage errors, not a wider census bell curve.

6. **Physical-host and process-container populations differ.**
   The requested trace emitter unit is a process/container incarnation.
   Deduplicating physical host IDs among independently tracked containers and
   dividing by f does not estimate physical hosts. A host with m containers
   has inclusion probability `1-(1-f)^m`. For 100 hosts with 20 containers each,
   f=1/128 gives a naive expected physical-host estimate of 1,858.

7. **Reusing rank identities invalidates independent-rank error bars.**
   If all 20 containers on each of 100 hosts reuse the same census rank, the
   process population estimate remains unbiased but is clustered. In seeded
   f=1/8 tests, its actual standard deviation was 529 versus an independent-rank
   prediction of 118; a nominal 95% Gaussian interval covered only 33.8%.
   Distinct simultaneously live process identities are needed for the
   advertised independent-rank uncertainty.

## Census formulas and small counts

For independent ranks, complete visibility and fixed population N:

```text
R ~ Binomial(N,f)
N_hat = R/f
E[N_hat] = N
Var[N_hat] = N(1-f)/f
estimated_SE = sqrt(R(1-f))/f
```

The Gaussian approximation should be available only at a reasonable tracked
count, such as R>=30; full f=1 has zero hash-sampling error and should be shown
as a point mass. A zero sparse count is a valid point estimate of zero but
does not establish an empty population or zero uncertainty.

For R=0, an exact one-sided 95% nonrejected integer population upper limit is
`floor(log(0.05)/log(1-f))`. At f=1/128 this is 381; no low-count Gaussian
interval should be synthesized. Returning the point estimate with unavailable
Gaussian uncertainty is a compact honest API.

In 60,000 seeded trials at N=100,000 and f=1/128, the plug-in Gaussian interval
covered the true population 94.90% of the time. This calibration tests hash
sampling only; it does not establish actual membership completeness.

## Original same-rank recording law

The original design's same complete cohort had
`A | R ≈ Binomial(R,min(1,T/R))`; an independent next cohort reading the old
census had `A_new | R_old ≈ Binomial(N_new,min(f,T*f/R_old))`.
Treating its census and admission ranks as independent would be incorrect.
The new independent admission design changes the draw semantics but retains
the same conditional current-cohort binomial count law when tracked members
use the same probability r.

Priority/bottom-k sampling itself is not universally invalid. For a fixed
eligible cohort, the (k+1)th excluded rank supports useful conditional
threshold estimators. Extending that result to frozen heterogeneous admissions,
adaptive censuses, rank-dependent leader timing, asynchronous terminal trims
and changing membership needs a careful leave-one-out proof. Independent
Bernoulli rounds avoid relying on that substantially more complex argument.

## Validation artifacts

- `checks.py` / `results.json`: exact finite census design enumeration,
  60,000-trial Gaussian calibration, direct shared-rank checks, clustered rank
  checks and deterministic history/ghost counterexamples.
- `check_weighting.py` / `weighting-results.json`: 60,000 trials per weighted
  workload scenario, independent census/admission draws, endogenous census
  probability, lazy delivery, skipped rounds, duplicate reads, stale-count
  q, exogenous exits and deliberate entropy/idempotence failures.
- `weighting-proof.md`: derivation, metadata requirements, trace slicing,
  conditional variance, positivity and delivery assumptions.

These files are review evidence separate from the implementation PR.
