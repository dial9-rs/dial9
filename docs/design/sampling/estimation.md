# Population and recording-count bounds

A target is an expected recording count. All bounds below assume uniform
independent 64-bit ranks, unique rank identities among simultaneously live
processes, complete fresh membership, stable population, and no partitions,
ghosts, manual recorder resumes or config mismatch. Four flat census readings
do not establish those assumptions. Startup can deliberately reuse an old
settled arm, so arbitrary population changes have no unconditional error bound.

## Census estimator

Let `f = 2^-k`, `N` be live processes, and `R` be the count whose rank lies in
that fixed slice. Under the stated model:

```text
R ~ Binomial(N, f)
N_hat = R/f
E[N_hat] = N
Var[N_hat] = N(1-f)/f
relative_SD = sqrt((1-f)/(N*f))
```

Default `minimum_population=0` gives f=1: a complete census has no hash-sampling
error. Missing/stale membership still causes error. Sparse census is optional:
choose the largest k such that `minimum_population/2^k >= 32*T`. This requires
an actual lower bound for each complete scope/dimension tuple, not an upper
bound or an application-wide population.

If `N >= minimum_population`, expected tracked count is at least 32T. With
`mu=N*f`, the probability of no tracked member is `(1-f)^N <= exp(-mu)`.
For `0 < epsilon <= 1`, a standard multiplicative Chernoff bound gives

```text
P(|R-mu| >= epsilon*mu) <= 2*exp(-mu*epsilon^2/3).
```

A violated lower-bound assumption cannot be fixed for already untracked
processes by publishing another cutoff. For f=1/128 and N=15, the chance of no
tracked member is about 88.9%. Default full tracking avoids that tradeoff.

## Arm and recording counts

Use integer rank space `M=2^64`, tracked-space width `L=M/2^k`, and threshold

```text
a = min(L, floor(T*L/R))    when R>0
```

Zero observed members never establishes a usable arm. A prior compatible
settled arm can remain usable for continuity. Selecting `rank<a` accounts for
both tracking and admission; there is no second independent participation draw.

The distinction between two populations matters:

- **Same census cohort:** conditional on R, its R tracked ranks are uniform in
  the tracked interval, so `A | R ~ Binomial(R, a/L)`. For R>=T its conditional
  mean lies between `T-R/L` and T; discrete-rounding error is negligible. In the
  continuous approximation `A | R ~ Binomial(R,T/R)`, with variance `T(1-T/R)`.
  The census and selection share rank, so treating them as independent would
  calculate the wrong distribution.
- **Independent next cohort:** an old complete cohort publishes a, then
  independent new identities read it. `A_new | R_old ~ Binomial(N_new,a/M)`.
  Its mean is `N_new*E[a/M]`; it is not universally exactly T. A smaller old
  cohort followed by a larger new cohort over-admits, and the reverse under-admits.
  Sparse census adds uncertainty through random R_old. At large R the extra
  variance is approximately `T^2*(1-f)/(N_old*f)`.

For complete all-tracked old/new cohorts of the same N=100,000, threshold
rounding aside, the new count is `Binomial(N,T/N)`. Exact numerical references
before terminal trimming are:

| Target | Mean | Standard deviation | Central 95% count interval | Default reconciliation limit | Probability above limit |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 15 | 15 | 3.873 | 8–23 | 27 | 0.1714% |
| 500 | 500 | 22.305 | 457–544 | 568 | 0.1296% |

Thus sixteen rather than fifteen is ordinary, useful variation. Relative count
noise shrinks as target grows. A target of fifteen does not imply a narrow
percentage-error guarantee. The 95% interval above spans roughly -47% to +53%.

For a binomial count with mean lambda, its moment generating function is at
most `exp(lambda*(exp(t)-1))`. Markov's inequality with `t=log(b/lambda)` gives

```text
P(A >= b) <= exp(-b*log(b/lambda)+b-lambda), b>lambda.
```

At b=28 and lambda=15 this conservative bound is 1.1375%, versus the exact
0.1714% tail. These probabilistic startup bounds are separate from the
conditional eventual-trim argument in [proofs.md](proofs.md).

## Reproduce and inspect distributions

```sh
python3 docs/design/sampling/fleet_simulation.py
```

`simulation-results.json` contains 50,000 seeded trials for each target and
tracking policy, histograms of same-cohort and independent-next-cohort counts,
population error percentages/quantiles, exact binomial references, adversarial
protocol scenarios, and request counts. Trials use Python 3.12's binomial
sampler; the cohort laws above account for shared rank rather than simulate a
second independent census draw. Empirical percentages have Monte Carlo error.

A stale census, two-bucket crash retention, correlated overlapping restarts,
failed writers or changing cohorts can dominate this statistical noise. Treat
those as protocol/freshness conditions, not another term hidden inside a bell
curve. Shrinkage never re-enables already unarmed or trimmed processes.
