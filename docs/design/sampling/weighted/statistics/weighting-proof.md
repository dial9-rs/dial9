# Fleet flamegraph expansion weights

The statistical unit is a live process/container incarnation that could emit
Dial9 traces. Distinct physical machines are a separate estimand.

## Recommendation

Use one census rank for sparse tracking, a fresh independent admission draw
per incarnation, and a fresh independent coin for each published trim round.
Record the startup expansion weight and every subsequent survivor weight
change in the trace, with a weight checkpoint at each new trace file boundary.
Retained census observations are useful validation evidence, but are not the
inverse inclusion probability of a trace.

## Startup weighting

For a fixed potential emitter i:

- C_i indicates its inclusion in the census slice, with probability f>0.
- r_i>0 is its conditional admission probability given tracking. It can be a
  function of the observed roster, the past census, and C_i.
- A_i is a fresh admission coin independent of the census and previous
  admission/trim draws, with conditional success probability r_i.

The startup expansion variable is

```text
Z_i = C_i A_i / (f r_i).
```

First condition on C_i and the information used to compute r_i:

```text
E_A[Z_i | C_i, past] = C_i / f.
```

Then average over the census design, obtaining E[Z_i]=1. This cancellation
does not require the realized roster probability to be independent of its
census. It does require positive admission probability for the emitter and
an independent admission coin. Reusing a stable census rank as that coin does
not provide this simple proof.

For nonzero selected processes the trace starts with expansion weight
`1/(f*r_i)`. A realized process without a trace contributes zero to the
estimator in that draw. The expectation over the sampling design recovers
its population contribution.

## Asynchronous trimming

At any later application event, q>0 is sealed from previously observed
information before that round's new coin is revealed. A selected process
draws B with conditional success probability q. Set

```text
Z_i' = Z_i B/q.
E[Z_i' | all prior events] = Z_i.
```

The sequence of expansion variables is a martingale. Consequently, a weighted
sum at a fixed time has the correct expectation for the live population, under
the assumptions below. q may depend on a stale count, ghosts, the current
selected set, a previous draw, or that emitter's previous admission: these
are all information from before the new coin.

Different processes can receive the round at different times or miss it.
Weight each process by the q values it actually applied. Every round must
be applied at most once. Processing the same round again with its already
revealed coin and dividing the weight again is biased. Reusing admission
entropy as a trim coin is also biased.

The applied factors are sequential conditional rates. Their product need
not equal the emitter's unconditional marginal probability of appearing in
a trace. `expansion_weight` accurately describes the inverse product without
making that stronger claim.

## Flamegraph construction

Let y_i,b(t) denote unit i's contribution to stack bucket b at time t, after
accounting for the profiler's own sample frequency/duration. The fleet
estimator is

```text
Y_hat_b(t) = sum over available sampled units i of expansion_weight_i(t)
            * observed_contribution_i,b(t).
E[Y_hat_b(t)] = sum over live potential emitters i of y_i,b(t).
```

Use the weight in force when the sample was collected. A trim survivor's
increased weight applies to subsequent contributions, not its earlier samples.
Recording the source clock timestamp of a weight change and repeating the
current weight in each trace file allows isolated slices and rotated files to
be analyzed without inventing missing prior state.

A failed trim coin must synchronously write a known terminal weight of zero
before asynchronous recorder stopping. Ignore samples collected after that
decision, including frames produced while a stop monitor catches up. The
reviewed implementation now does this. A null weight represents unknown
coverage, rather than zero, and frames beyond the recorded checkpoint also
lack evidence for a weight.

The final normalization of expanded buckets into percentages is a ratio
estimator. Even if expanded bucket totals are unbiased, normalized percentages
are not exactly unbiased at finite sample size.

## Assumptions and unsupported cases

1. All potential emitters have positive census/admission support. Cold-start
   `Disabled` cohorts, target zero, invalid configurations, and deliberately
   uninstrumented processes cannot be reconstructed from others' traces by
   a finite inverse weight.
2. Fresh admission and trim entropy is independent of census ranks and
   workload. Round q and delivery/application timing cannot depend on that
   round's not-yet-revealed coin. Process lifetimes are not caused by the new
   profiling coin.
3. Traces/samples are delivered completely, or their delivery probabilities
   are separately modeled. These weights cannot repair unknown buffer drops,
   manual recorder pauses, corrupt trace files, or failed uploads.
4. Workload within an observed process is sampled representatively. Fleet
   weights do not replace the profiler's frequency/duration accounting.
5. Hash ranks have the advertised uniform design. Reusing one rank for
   simultaneous process/container identities introduces clustering:
   expectation can remain correct, but independent-rank Gaussian uncertainty
   does not.

A positive `AssumePopulation` fallback maintains sampling support when the
roster is unavailable. Its assumption affects profiling volume and variance,
but the recorded realized rate still supplies a valid expansion factor when
the fresh admission coin is independent.

## Count and error distributions

For a simultaneous trim of A independent selected units using q=T/A:

```text
survivors | A ~ Binomial(A,q)
mean = T
variance = T(1-T/A)
```

The reconciliation limit is probabilistic. While only some processes have
received a round, their Bernoulli survivor count is added to the processes that
have not yet applied it; there is no immediate cap at every time slice.

For selected expansion weights w_i, the conditional variance introduced by
a common q round is

```text
Var(sum survivors' expanded weights | selected set)
  = (1-q)/q * sum_i w_i^2.
```

This is an additional conditional variance, not the complete estimator
variance. Heterogeneous large weights and small counts need not have a good
Gaussian approximation. An empirical estimator histogram with a clearly
labeled moment-matched Gaussian reference is honest; a universal Gaussian
confidence bound is not.

## Independent checks

Run `python3 check_weighting.py`. `weighting-results.json` records 60,000
seeded trials for heterogeneous static admission and an admission probability
that depends on each trial's observed census. Both cases exercise lazy round
delivery, different delivery rates by cohort, skipped rounds, duplicate
revision reads, ghost-biased q, and exogenous exits.

In the heterogeneous case true fleet workload is 2,080. Sequential expansion
estimated 2,068.49 after asynchronous rounds (Monte Carlo SE 10.37), while
`N_hat / profiler_count` scaling estimated only 681.14. In the census-dependent
case the expanded result was 2,065.31 (Monte Carlo SE 13.38), while ratio
scaling estimated 616.64. All expanded means were within 2 Monte Carlo
standard errors of their truth in this seed.

Negative controls reused admission entropy or reapplied the same trim
revision. Both produced an expanded mean of about 200 for a true population
of 100, demonstrating why entropy separation and round idempotence matter.

These are seeded model checks and a proof under explicit assumptions, not an
S3 latency benchmark or evidence of complete trace delivery in production.
