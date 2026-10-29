# Rotating-window estimator and trace weights

This is a mathematical argument under explicit sampling assumptions, plus a
finite safety abstraction. It is **not** a refinement proof of Rust, hashing,
S3 delivery, scheduler progress, or election liveness.

## Predictable cutoffs and the continuous-rank theorem

Fix one completed window and its observation opportunities. Run `i` is alive
at its opportunity, has reporting cutoff `c_i > 0`, and draws independent
`U_i ~ Uniform[0,1)`. Condition on all earlier-window information. Every `c_i`
must be independent of **all** current-window ranks, not merely its own.
Report iff `U_i < c_i`.

Let `theta` be the `(K+1)`st reported rank, or 1 after verified prefix
exhaustion. Retain strictly smaller ranks and estimate:

```text
N_hat = sum over retained i of 1 / min(c_i, theta)
```

For each `i`, condition additionally on the other current ranks. Let `T_i`
be the `K`th reported rank among the other runs, or 1 if fewer than `K`
exist. Run `i` is retained exactly when `U_i < min(c_i,T_i)`; when retained,
global `theta = T_i`. With continuous ranks, `T_i > 0` almost surely, so:

```text
E[contribution_i | other ranks, earlier information]
    = min(c_i,T_i) / min(c_i,T_i) = 1.
```

Linearity gives exact unbiasedness for the number alive at their opportunities.
No extra division by a common reporting fraction is valid. Heterogeneous
cached fractions are permitted because each immutable key records its actual
cutoff.

The controller preserves predictability by freezing current-minute lotteries
and making every learned change effective only at a future boundary. It
processes a completed source at most once. A current GET, election, report
retry, or roster refresh cannot redraw a current rank or alter its cutoff.
Fresh ranks in every window avoid feeding a stable rank back into its own
future cutoff. Domain-separated SHA-derived draws are treated as independent
uniform draws under an ideal-hash/random-run-seed assumption; that assumption
is not proved by these resources.

A counterexample explains the stronger independence condition. With two runs,
`K=1`, `c_1=1`, and `c_2=U_1`, a cutoff chosen before its *own* draw still yields
expected estimate 1.5, rather than 2.

## Implemented integer ranks: a rare zero-boundary qualification

Rust uses 64-bit integers `R_i`, exact integer cutoff `C_i`, and strict
`R_i < C_i`. For a positive leave-one-out integer threshold `t`,
`P(R_i < min(C_i,t)) = min(C_i,t)/2^64`; the same cancellation applies.
Excluding all boundary ties is correct for this strict threshold rule.

The old proof sketch incorrectly extended the argument to `t=0`. At zero,
inclusion probability is zero and the expected contribution is zero, not one.
With tiny eight-value ranks, three runs, and `K=2`, the expected numerical
estimate is `189/64`, not 3. The exact fixture exposes this instead of hiding
it behind continuous simulations.

Core rejects a zero boundary as an **unusable observation**. It does not divide
by zero, invent a population, or refill beyond the page cap. Consequently,
exact unbiasedness conditional on usable discrete samples is not asserted.
Under ideal uniform ranks the exception requires at least `K+1` zero ranks
in the observed prefix; its probability is extremely small for ordinary
fleets with `K=480`. A generic leave-one-out bound for `n` opportunities is
`P(Binomial(n-1,2^-64) >= K) <= choose(n-1,K) / (2^64)^K`.
Floating-point accumulation and cutoff conversion add numerical approximation.

## Completed-window mean and temporal qualifications

For aligned UTC window `W` of width `H`, choose each run's observation instant
`S_i` independently and uniformly in `W`, separately from its rank, controls,
and exogenous lifetime. A start after `S_i` skips that opportunity.

```text
E[N_hat] = sum_i length(lifetime_i intersect W) / H
         = mean concurrent live trace-emitting processes during W.
```

It is not an instantaneous count. A process alive for 60 seconds across two
windows has independent opportunities in those windows, not an enforced
exactly-one registration; its expected contribution across them is one.

Constant per-run clock offset `delta_i` changes the real interval sampled by
the label to `W-delta_i`. Visibility margins do not remove this timing shift.
A run alive exactly in real `[0,60)` with either ±5s offset contributes
expected `55/60` to label `[0,60)`. Stable fleets are insensitive to that
example, while churn can introduce temporal alignment error; no universal
relative or instantaneous bound is claimed.

The proof assumes all eligible reports arrive before the stable observation.
Client deadlines, closure margins, and bounded skew do not prove that crashed,
paused, blocked, or uncertain writes arrive. Unobserved delivery/dispatch loss
can bias counts downward. A 30% independent report-loss experiment converges
near 70% of the true population; that bias is not Gaussian sampling error.
Immutable stable prefixes avoid requiring a multi-page atomic snapshot under
the complete-delivery assumption. Truncation, malformed keys, repeated
identities/cursors, zero boundary, and budget exhaustion degrade observations.

## Profiler control and correctly weighted traces

Profiler observations have independent instants and ranks, cutoff 1, and
bottom-`P` boundary estimation. Sampler-driven thinning activates at a future
boundary, keeping observation time independent of a same-minute GET.
Only the completed source cohort (`born_window <= source_window`) is eligible;
new starts absent from that cohort are not trimmed by its token.

For startup probability `p_i > 0`, let startup selection be `A_i` and weight
`A_i/p_i`. Condition on the full state before each fresh independent trim coin
`B_i ~ Bernoulli(q_i)`, with actual `q_i > 0`. Multiplying surviving weight by
`1/q_i` preserves its conditional expectation:

```text
E[(old_weight * B_i / q_i) | earlier selections and control] = old_weight.
```

Thus sequential weights remain `(1/p_i) * product(1/q_i)` for survivors,
including adaptive trim probabilities computed from earlier samples.
Multiplying each contribution by an exogenous CPU workload preserves the same
conditional argument, even when workloads and lifetimes differ between runs.
This assumes that the workload/lifetime being estimated does not change in
response to the run's profiling coin. Informative departures or trace loss
need their own observation model; count simulations do not prove workload
conservation in those cases.
An eligible run applies a token at most once. A missed activation is skipped,
not applied retrospectively; such a run retains its actual untrimmed weight.
There is no replacement by `N_hat / observed_profiler_count`.

The metadata `Source` exposes this scalar at evaluation time. Finite positive
representability, independent coins, positive support, and trace delivery are
conditions. The segment uses its evaluated snapshot; changes within a segment
remain the explicitly accepted approximation. Overflow becomes unknown metadata.
A default-disabled cold cohort has `p=0` and no profiling support. Discovery
does not re-enable it. A surviving one-process remnant from a million-process
fleet has only `15/1,000,000` initial profiling probability: census recovery
does not make traces appear when all admitted profilers departed.

## Bounded work and conditional progress

A monotonic source reservation CAS precedes either LIST. Takeover preserves it;
failures/uncertain outcomes never reserve the same completed source anew.
Each channel uses `ceil((keep+1)/1000)` logical LIST requests and at most
`keep+1` keys. At target 15 the total is 2 logical LIST calls / 546 keys.
Result publication is fenced on the reservation object's version. Reservation
is distinct from a valid observation and never freshens population source age.

One fixed uniform 300s phase determines periodic slots; recording runs use its
60s subslots. Quiet slots are every fifth active slot, preserving the maximum
300s healthy opportunity gap even when recording stops. Any successful reader
can contest expired leadership, independent of selection or recording.
These are opportunities under healthy scheduling/clocks/requests, not a
wall-clock election completion proof.

Verified complete sparse/empty census observations raise future `c` by
`max(min(1,2*c_old), K/N_hat when positive)`. Unusable observations hold it.
Zero is unknown; source-aged admission expires after 600s. This enables an
explicit recovery ramp without feeding unknown storage results into population
zero or amplifying PUTs to repair LIST failure. Convergence remains statistical
and delayed by future activation and cache dissemination.
