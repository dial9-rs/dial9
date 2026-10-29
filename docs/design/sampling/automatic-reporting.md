# Automatic population discovery

Status: implemented revision, October 28, 2026; local integration validated October 29.
Source: `aaa8c7208a0d4e6a76608f0d8fc76b4d6bfa42d7`.
This replaces the caller's `minimum_population` assumption. The new
[protocol evidence](automatic/internals/proofs.md) covers this revision.
The archived schema-3 proofs and visualization describe the preceding protocol.

## 1. User-facing documentation

Configure the desired number of profiling processes in each pool. Dial9 learns
the pool's population; callers do not supply its current size or a minimum size.
The counted unit is one live process/container run that could emit Dial9 traces.
Several such processes on one machine count separately.

Identify a pool with storage configuration and additional named dimensions.
The S3 integration takes the existing `S3Config`; its bucket, prefix and
`service_name` isolate services automatically. Callers add environment, cell,
instance type and other grouping keys. There is no separate `scope` or required
caller-provided service dimension.

```rust,ignore
let s3 = dial9::s3::S3Config::builder()
    .bucket("trace-bucket")
    .service_name("checkout")
    .build();

Sampling::builder()
    .add_dimension("environment", "production")
    .add_dimension("instance-type", "m7i.large")
    .add_dimension("az", "us-east-1a")
    .target(15)
    .if_failed(StartupFailure::disabled())
    .build()
    .with_storage(async {
        S3SamplingStore::builder()
            .config(s3.clone())
            .build()
            .await
    })
    .start_application(|status| {
        let SamplingStatus { enabled, .. } = status;
        let (recorder, runtime) = configure_application(enabled, &s3)?;
        status.register_recorder(&recorder);
        runtime.block_on(application_main());
        // Drop the runtime and drain the recorder before returning.
        Ok(())
    })
```

When a pool has no usable reporting policy, processes register their presence
while its population estimate forms. The leader then publishes a reporting
fraction aimed at about `32 * target` census reporters: 480 for target 15.
Processes use a stable hash to decide whether to continue reporting.
Others stop membership heartbeats and occasionally read the shared roster.
The hash is stable within an epoch; each reporting-fraction change uses fresh
epoch entropy to avoid selecting and measuring with the same hash sample.
Processes joining an established pool can read its policy first and avoid an
initial membership write if neither reporting nor profiling requires one.

Reporting and profiling are separate decisions. Every process can be selected
to profile at startup with probability `min(1, target / population_estimate)`,
using randomness independent of its reporting hash. A change to the reporting
fraction changes census participation, not an existing profiling decision.
Existing profilers can still be randomly thinned after an overshoot; processes
that started off or were thinned remain off under the sampler's policy.

Examples below assume a settled, accurate estimate and a healthy backend:

| Processes in the pool | Reporting fraction | Expected census reporters | Startup profiling probability |
| ---: | ---: | ---: | ---: |
| 100 | 1 | 100 | 15% |
| 1,000 | 0.48 | 480 | 1.5% |
| 100,000 | 0.0048 | 480 | 0.015% |

Profilers also report their presence so the leader can detect overshoot. Those
outside the census subset add writes but are excluded from the population
estimate. Census reporters and profilers are deduplicated, not added twice.
The reporting budget is approximate, not a fixed allocation or write cap.
Small pools continue to have every process report.

Every nonzero-target process retains a lightweight private worker for roster
reads and recovery. Steady membership writes concentrate in the reporting
subset and current profilers; reads still scale with the whole population.
Bootstrap and reporting-policy handoffs temporarily require more writes.
The application continues to own its runtime, callback and recorder shutdown.
Registered recorders use ordinary `disable()`; no permanent-disable API is added.
Explicit application-controlled enables override the sampler and its weighting
assumptions, as in the current recorder contract.

Startup remains bounded by the existing 1000ms default. Discovery cannot promise
a complete census within that budget. Keep the explicit startup failure policy:
with `disabled()`, a cold or unavailable-roster startup stays off even after the
census settles. This cohort has zero profiling support and cannot be reconstructed
from later traces. An optional positive fallback can provide support, but its
configured population is a fallback volume choice, not a requirement for census
discovery. This revision does not introduce delayed profiling enablement.

## 2. Reference guide and implementation details

### Pool dimensions

Pool identity is the canonical sorted tuple of additional dimension names and
values within the selected store namespace. Names are unique and nonblank;
values are nonblank. Input ordering does not change the pool.

The native S3 store isolates configured services in its coordination prefix.
Custom stores sharing a physical backend provide equivalent namespace isolation.
Validate the caller's dimension tuple before asynchronous storage initialization;
the storage trait does not acquire service-discovery or dimension-merging hooks.
Sampling runs before the recorder exists and does not infer identity from it.
The dimension hash remains known if initialization fails, interpreted relative
to the configured store. Existing trace metadata supplies service/bucket/prefix.

### Reuse S3 upload configuration

Reuse `dial9_destinations_s3::S3Config`, also re-exported as `dial9::s3::S3Config`.
It is already cloneable and describes the bucket, service name, optional region
and optional prefix. Pass it to the S3 storage constructor inside
`with_storage(async { ... })`. This is the same initialization boundary for S3
and custom stores; the future resolves to the initialized store or an error.

`with_storage` itself only captures the future. The synchronous
`start_application` drives it on the sampler's owned private runtime within the
startup budget, then calls the application callback with its startup status.
Creating or dropping unused startup configuration performs no I/O. Preserve
the existing ability to borrow local configuration and supply custom SDK-client
initialization. This replaces the unpublished `with_backend` boundary with
`with_storage`.

Use the same config for recorder construction. The existing S3 recorder adapter
already publishes its `service_name` metadata; do not add a second service-name
configuration or rename that metadata key. Coordination uses that configured
service identity without repeating it in the sampling builder.

Coordination lives under
`{configured_prefix}/_dial9_sampling/service={sha256(service_name)}/`, or
`_dial9_sampling/service={sha256(service_name)}/` when no prefix is configured.
The configured prefix is preserved verbatim, as in trace keys. Use the config's
bucket and region semantics, including bucket-region discovery when needed,
within the bounded startup budget. Provide small typed read-only config
accessors for the adapter rather than parsing `as_metadata()`. Keep the S3
dependency optional.

Instance path, boot identity and custom trace `key_fn` describe individual
uploads, not sampling-pool dimensions or coordination key layout. Keep sampling
CAS operations, single-attempt conditional writes and uncertain-write readback
independent of the uploader's circuit breaker and retry behavior. Config reuse
does not move uploader runtime resources into the sampler or require sharing an
initialized SDK client across runtimes.

### Process identities and independent decisions

Each simultaneously live trace-emitting process gets a fresh private run identity.
Do not use a physical hostname alone for multiple containers or processes.
Coordination can start before recorder construction: assign its census rank and
membership ownership once using the current fresh run identifier. Recorder
registration does not rerank the process or rename owned keys. Traces keep their
existing `boot_id`; add no new trace identity field.

For a process with census rank `h` in `[0, 1)`:

```text
census_reporter = h < reporting_fraction
heartbeat_required = census_reporter || currently_selected_to_profile
profile_at_startup = fresh_admission_draw < p
p = min(1, target / usable_population_estimate)
```

The census hash is stable within a reporting epoch, with separate entropy from
admission and each thinning round. A new epoch's fresh ID domain-separates its
hash after its fraction has been sealed from prior observations. This redraws
reporter membership without changing profiling. Neither a reporting-policy
change nor a backend retry reveals another admission coin.

### Population estimation and the reporting controller

At a settled reporting epoch with fraction `f`, count distinct visible
census-eligible processes as `R`, then publish `population_estimate = R / f`.
Filter by the epoch's fraction even if extra profilers have membership records.
Deduplicate heartbeat buckets using the existing terminal-state precedence.
Never publish a count from failed or incomplete pagination.

Start discovery with `f = 1`. Once the bootstrap census settles, propose
`f_next = min(1, reporter_budget / population_estimate)`, with
`reporter_budget = 32 * target` as an internal default. Use an integer threshold
representation; observations record the fraction actually represented.
Remove the caller's minimum-size assertion and dyadic eligibility ceiling.
Target zero continues to perform no sampler I/O.

Controller hysteresis: keep the fraction while the healthy reporter
count lies between half and twice the budget. After three consecutive complete
counts outside that band, propose a new fraction using the estimate above.
A complete zero-reporter count triggers full reporting recovery (`f_next = 1`)
without waiting for three counts. If `f` is already 1, a small nonzero count
does not trigger a pointless transition. Do not adjust the fraction using a
failed census. The executed models and simulations document these tuning values
and their limits.

For a fixed fraction chosen independently of the ranks being measured, complete
membership, and independent uniform hashes:

```text
R ~ Binomial(N, f)
E[R/f] = N
Var(R/f) = N * (1-f) / f
relative standard deviation = sqrt((1-f) / (N*f))
```

With `N = 100,000` and `f = 0.0048`, relative standard deviation is 4.55%;
the Gaussian 95% reference is approximately +/-8.92%. A full census has no
hash sampling error. With few reporters, use discrete uncertainty rather than
a Gaussian approximation.

These equations do not apply to adaptive changes chosen from the same hashes.
The [Theta Sketch framework][theta] explicitly conditions its guarantees on the
threshold-selection rule (sections 3.2 and 3.6). The rejected stable-rank control
has roughly `1/480` relative feedback bias in the tested interior adjustment.
The implementation seals a new fraction before choosing its fresh epoch hash
salt, giving the conditional binomial argument at fixed observation times.
The data-dependent promotion time, asynchronous delivery and changing fleet
remain separate limitations; this is not an unconditional error bound.

Hash uncertainty describes visible membership, not exact instantaneous liveness.
Late reports, failed cleanup, short-lived processes, stale observations and
partitions introduce separate error. Retain the existing bucket, clock-skew,
complete-pagination and census-age checks.

### Reporting-fraction handoff

Give each reporting policy an epoch identifier. Keep one active epoch and at
most one pending epoch in the same CAS-fenced roster as leadership. Store the
last usable population observation separately from the pending fraction.
Reporting epochs and thinning rounds have independent identifiers and purposes.

1. The leader seals a pending epoch with its new fraction and a collection delay.
2. Processes learn it through their existing jittered roster reads. Eligible
   processes immediately schedule a heartbeat for that epoch; they do not wait
   for the next five-minute heartbeat bucket. Old reporters continue reporting
   the active epoch. Profilers report in both epochs during the handoff.
3. Membership keys distinguish epochs. The leader lists and deduplicates each
   epoch separately; it never divides an old epoch's count by a pending fraction.
4. Promotion requires the collection delay and a complete, guarded pending
   census. The delay is `5*poll + 2*skew + backend timestamp uncertainty`;
   promotion also waits for four plateau observations. It covers quiet polling,
   immediate reports and bounded operations under healthy delivery. Time passing
   cannot certify that unavailable processes reported.
   Apply the existing settling checks as heuristics, not completeness proofs.
5. A CAS publishes the new active epoch and its population observation together.
   Later roster reads end old-epoch heartbeats. Old keys may expire normally.

During collection, startup uses the last usable estimate subject to its existing
freshness/failure policy. Pending counts are diagnostic, not admission inputs.
If collection fails, retain the active epoch. Takeover preserves any pending
epoch and its collection deadline; only the current leader can promote it.
At most two epochs report concurrently, avoiding an accumulating transition queue.

The same handoff works when increasing or decreasing the fraction. It explicitly
handles both delayed new reporters and surviving old membership objects.
No handoff applies a thinning round or changes an existing trace weight.

### Shrink and loss of all reporters

Any compatible process may attempt leadership after observing an expired lease,
including a quiet nonprofiler. Use jitter, a fresh roster reread and the existing
conditional-write fencing. Reporter eligibility must not gate recovery.
An expired lease alone does not require full reporting: the replacement leader
first attempts a census of the active epoch.

A valid zero-reporter census is loss of population visibility, not proof of an
empty fleet. The leader starts a pending `f = 1` epoch; quiet live processes
discover it and register. Missing or incompatible rosters follow bootstrap or
policy-mismatch behavior respectively, rather than rewriting another pool.
Do not replace the last usable population with zero or interpret it as a
probability of one for new profiling admissions.

For example, a pool shrinking from 100,000 processes to 100 while retaining
`f = 0.0048` has only 0.48 expected census reporters. It can lose every reporter.
Recovery therefore cannot depend on a surviving reporter or a profiling process.
Its healthy-backend latency includes quiet polling, election, handoff collection
and census settling. Crashed membership can delay recognition until expiration.
An arbitrary partition has no unconditional recovery-time or profiler-count bound.

### Profiling control and post hoc weighting

The active count includes every visible sampler-selected profiler, whether it
belongs to the census subset or not. It describes selection, not proof that a
recorder emitted samples. Use recorder/trace evidence for actual profiling.
Use the active epoch for overshoot counts until promotion; pending-epoch records
do not add a second copy of a profiler or supply an incomplete trimming census.
Existing overshoot detection, independent Bernoulli thinning and once-per-round
application continue to operate on this count.

Decoupled admission simplifies the trace expansion factor:

```text
initial weight = 1 / actual_startup_probability
after surviving an applied thinning round: weight *= 1 / q
after failing a thinning round: weight = 0, then disable registered recorders
```

The current reporting fraction does not appear in this factor. Never recompute
an existing weight as `population_estimate / profiler_count` or rescale it when
reporting membership changes.

For an independent admission coin `A` drawn after choosing positive `p`,
`E[A/p | information used to choose p] = 1`. Thus population-estimate bias or
staleness affects profiling volume and variance, but does not invalidate this
conditional expansion-weight argument. Each independent survived thinning coin
similarly preserves expectation after multiplication by `1/q`. Exogenous
workloads/lifetimes, positive support and usable trace delivery remain assumptions.

The metadata source supplies only the current scalar weight and pool identity.
Keep the accepted contract: the value is correct when evaluated; treating it
as constant across a segment is an approximation. Existing boot identity and
clock metadata provide identity and time; add no weight history or extra clock.
Weight sample mass before summing stacks for fleet flamegraphs.

Retained census observations additionally record reporting epoch, actual `f`,
`R`, listing interval and existing freshness/quality information. Historical
population lookup uses the observation's fraction, never today's roster fraction.
These observations estimate visible process population at the documented
resolution; they are not a prerequisite for decoding a segment's scalar weight.

### Compatibility and evidence

This is a protocol change: the previous schema treats the reporting fraction as
immutable configuration and excludes nonreporters from profiling and all I/O.
Schema 4 uses new `v3/` coordination keys; old and new workers
must not share leadership or interpret each other's membership. Check published
API compatibility before removing `minimum_population`; the intended new
onboarding path never asks for it.
The dimension-only pool key also supersedes the old `(scope, dimensions)` key.
Replace `scope` with S3Config service isolation and extra grouping dimensions;
check public API compatibility rather than silently joining previously named pools.

Preserve the existing implementation evidence as an archive. New proofs and
simulations must identify this revision and must not inherit its results by
changing labels in the previous visualization.

## 3. Concrete changes and validation checklist

- [x] Remove the minimum-population requirement and local nonreporter startup
  shortcut. All valid nonzero-target processes initialize bounded coordination.
- [x] Remove `scope`; reuse `S3Config` service isolation without a repeated
  service setting. Canonicalize extra dimensions, reject duplicates and share
  the config with recorder construction.
- [x] Name the existing asynchronous factory boundary `with_storage`, checking
  published compatibility. Let the S3 store constructor take `S3Config` and
  initialize asynchronously inside that future. Expose the small typed config
  accessors needed to reuse storage settings.
  Keep custom stores and bounded worker-owned client setup.
- [x] Separate profiling admission from census eligibility; store the actual
  startup probability and remove tracking-fraction caps from fallback admission.
- [x] Add mutable reporting epochs, one pending handoff and epoch-qualified
  membership keys under the existing leadership CAS. Preserve fencing/readback.
- [x] Keep profilers visible outside the census subset, exclude their extra
  records from `R/f`, and allow quiet processes to recover leadership.
- [x] Update terminal cleanup so thinned nonreporters can eventually stop
  membership writes once stale active records cannot win. Keep ordinary disable.
- [x] Preserve scalar metadata and historical observations with the correct
  epoch/fraction; update onboarding and API compatibility notes.
- [x] Model handoff, zero-reporters recovery, stale CAS, terminal startup
  choices and duplicate thinning in TLA+. Include broken controls for mixed
  fractions, premature promotion and reporter-only recovery. Rust regressions
  separately exercise takeover and terminal membership precedence.
- [x] Simulate cold start, rolling starts, one start per minute, one-minute
  lifetimes, many containers, large growth/shrink, lost reporters, delayed polls,
  failed writes/cleanup and partitions. Show true live processes, estimated
  population, actual profiling processes, and total GET/PUT/LIST/CAS operations.
- [x] Measure adaptive-estimator bias, empirical error quantiles, profiler
  distributions and weighted heterogeneous workload. Label Gaussian curves as
  fixed-fraction references. Prove the conditional admission/thinning weights;
  do not require an exact target count or claim instantaneous census accuracy.
- [x] Add focused Rust, paused-time and Shuttle regressions for those protocol
  invariants; run the required workspace/feature/format/Clippy checks after coding.

[theta]: https://raw.githubusercontent.com/apache/datasketches-website/master/docs/pdf/ThetaSketchFramework.pdf
