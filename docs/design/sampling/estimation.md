# Population estimates: bounds and limitations

Admission uses conditional slots, independently of population estimates. The
estimate is a diagnostic for a complete scope/dimension tuple. Unknown estimates
must not be treated as zero or summed as if they covered the entire fleet.

## Stable population model

Assume that all `N` eligible processes participate, ranks are independent uniform
draws, the current bottom `k` ranks are available, and there are no stale records,
partitions, missed refreshes, or duplicate identities. For `N >= k`, let `U_k` be
the normalized kth smallest rank. In the continuous model:

```text
U_k ~ Beta(k, N - k + 1)
N_hat = (k - 1) / U_k
E[N_hat] = N
Var[N_hat] = N (N - k + 1) / (k - 2), for k > 2
```

These moments follow by integrating the beta density against `1/u` and `1/u²`.
For a given `u`, `P(U_k <= u) = P(Binomial(N, u) >= k)`. This gives exact
model quantiles without a normal approximation. If `q_a` is the ath quantile of
`U_k`, the central 95% relative-error interval is:

```text
[(k - 1) / (N q_0.975) - 1, (k - 1) / (N q_0.025) - 1]
```

For the default `k = 256`:

| True pool population | Relative standard deviation | Central 95% relative error |
| --- | ---: | ---: |
| 256 | 0.392% | −0.381% to +1.055% |
| 300 | 2.430% | −4.326% to +5.178% |
| 1,000 | 5.416% | −9.961% to +11.256% |
| 5,000 | 6.112% | −11.251% to +12.696% |
| 100,000 | 6.267% | −11.534% to +13.016% |

At 100,000 nodes the probability of error within ±10% is 89.147%, rather than
95%. Chebyshev's inequality gives a looser 95% bound of ±28.025% using only the
mean and variance. These are probability statements under the model, not
deterministic bounds for every fleet. `k = 2`, outside the public configuration,
has infinite variance in the continuous model.

The implementation uses 64-bit SHA-256-derived ranks, normalized by `u64::MAX`.
Uniform hashing is an explicit assumption. Under that assumption, the union
bound on a rank collision among 100,000 processes is `2.711e-10`; discretization
also makes the continuous formulas approximations to the finite-rank system.

For independent pools satisfying the model, the summed estimate is unbiased and
its variance is the sum of their variances. This does not justify summing only
the pools whose estimates are known. No finite total-fleet error bound follows
from a snapshot containing unknown pools.

Reproduce the numerical bounds:

```sh
python3 docs/design/sampling/estimator_bounds.py
```

## Simulation results

Run the production estimator against 512 seeded populations, each with 100,000
hashed identities. The test materializes each population's exact bottom 256
ranks, then reads them through the production engine and `SamplingStore`.
It does not model convergence, churn, or S3 capacity.

```sh
cargo test --release -p dial9-sampling --lib estimator_qualification \
  -- --ignored --nocapture --test-threads=1
```

The October 26, 2026 rerun evaluated 51.2 million hashes. Mean estimate: 99,570.36;
relative bias: −0.430%; empirical relative standard deviation: 6.521%. The
empirical 2.5th/97.5th percentile errors were −12.334%/+13.669%. The mean was
1.55 theoretical standard errors below the true value. This is consistent with
the model; it is not evidence of a universal ±14% bound.

Separately, full startup/maintenance simulations each started 100,000 nodes:

| Complete dimension tuples | Population layout | Profilers, target one per tuple | Pools with known estimates | Pools with unknown estimates |
| --- | --- | ---: | ---: | ---: |
| 1 | 100,000 | 1 | 1 | 0 |
| 1,000 | 100 each | 1,000 | 0 | 1,000 |
| 1,000 | Uneven sizes: 5,000, 300, 100, 17, 16 | 1,000 | 100 | 900 |

In the October 26 qualification rerun, the single-pool estimate was 108,464.42
(+8.464%). The uneven layout estimated 75,749.13 for the 77,000 nodes in known
pools (−1.625%); the other 23,000 nodes were in unknown pools. Neither result
establishes accuracy of the full 100,000-node sum across arbitrary dimensions.
Startup recording choices remained unchanged through three maintenance periods.

```sh
cargo test --release -p dial9-sampling --lib fleet_qualification \
  -- --ignored --nocapture --test-threads=1
```

These tests use paused time, fixed 1 ms operation latency, and unlimited request
concurrency. Their 4 ms virtual startup decisions are protocol measurements, not
production latency predictions. Heavy qualification tests are ignored by the
ordinary suite and must be run explicitly.

## Freshness, completeness, and scan progress

A successful refresh records its completion time in internal diagnostics. On a failed/timed-out
refresh, the estimate, registration confirmation, and timestamp are cleared.
An in-progress scan reports an unknown estimate. Consumers exposing these diagnostics must report the known
estimate subtotal **and the number of unknown tuples**, never label that
subtotal as total fleet size. Below `k` live ranks, the eligible population is
unknown even after a complete LIST: a listing cannot prove that every eligible
process has participated. Admission remains independent of this diagnostic.

Each refresh reads at most eight pages within 500 ms. Completed-page cursors and
live ranks survive subsequent refreshes and timeouts; previous members heartbeat
before scanning. With a stable listing of `P` pages and sufficient request
capacity, finishing a pass requires `ceil(P/8)` successful refreshes, or more if
the deadline permits fewer pages per refresh. With `p >= 1` completed pages per
refresh, the bound is `ceil(P/p)` refreshes plus the participating processes'
registration/readback cycles. It is not a fleet-size-independent convergence
bound. Invalid continuation responses restart the scan and invalidate the
estimate. Tests cover stale-prefix progress both with zero latency and with
100 ms requests that force refresh cancellation.

LIST pagination is not an atomic fleet snapshot. Entries can arrive below a
saved cursor or depart while a pass progresses, so arbitrary churn/partitions
have no universal estimator bias bound. Cold pools can register more than `k`
records before observing the cutoff. The model's quantitative bounds require
complete participation and a stable current bottom-k sample; freshness metadata
alone does not certify those assumptions.

## Concurrent-start capacity

The 100k tests above use unlimited request concurrency. A separate ignored test
uses a FIFO store with eight permits and 1 ms service time. All 100k cold starts
can freeze off at the 500 ms deadline because the first reads queue behind the
entire fleet before their next required request. This is an overload model, not
an S3 throughput estimate. Run it explicitly:

```sh
cargo test --release -p dial9-sampling --lib capacity_qualification \
  -- --ignored --nocapture
```

Preparation's deadline is cooperative. Executor scheduling, synchronous caller
work and runtime/client construction can make observed wall time exceed the
requested budget. The virtual-time tests do not qualify real 100,000-node S3
throughput or startup latency. Once preparation returns, its profiling choice
is frozen; maintenance never runs a later admission phase.
