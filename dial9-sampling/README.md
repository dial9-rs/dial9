# Startup sampling

`dial9-sampling` makes one profiling decision at application startup from a
shared S3 population roster. An elected process maintains the census in the
background; excess profilers stop through normal recorder disable. The target
is an expected count. Overshoot triggers randomized thinning; the profiler count
has statistical error while storage and the census are healthy.

Enable `s3` for the native backend and `dial9` for recorder registration.

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
        S3SamplingStore::builder().config(s3.clone()).build().await
    })
    .start_application(|status| {
        let SamplingStatus { enabled, .. } = status;
        let (recorder, runtime) = configure_application(enabled, &s3)?;
        status.register_recorder(&recorder);
        // Run main, drop the application runtime, and drain the recorder here.
        Ok(())
    })
```

See the [complete runnable example](../dial9/examples/manual_sampling.rs) for
recorder construction, runtime attachment, trace uploading and shutdown.

## Application ownership

Call `start_application` from ordinary synchronous `main`, outside an entered
Tokio runtime. Storage initialization and maintenance use one private Tokio
worker, owned until the callback returns or unwinds. Construct SDK clients inside
the storage future; the application constructs its own instrumented runtime.
Futures and callbacks may borrow local values and need not be `Send` or `'static`.
Dropping unused startup configuration performs no I/O.

The default 1000ms cooperative startup budget covers runtime setup, storage
initialization and one roster GET. Blocking code cannot be interrupted. Cleanup
has a separate 500ms budget. A zero target or invalid sampling configuration
creates no storage or runtime. Dropping or returning `SamplingStatus` does
not retain or end coordination; `enabled` always describes the startup choice.

Register each recorder immediately after construction. The sampler stores a
recording handle and calls its normal `disable()` after a trim or callback
completion, including unwinding. Storage errors preserve recording; the sampler
never re-enables it. The application retains normal `enable()`/`disable()` control
and owns recorder shutdown. Explicit enables override the sampling decision
and are outside its count and weight estimates.

## Population and failure policy

A target applies independently to each storage namespace and complete dimension
tuple. Native S3 storage reuses the recorder's `S3Config`: its bucket, prefix and
`service_name` isolate coordination automatically. Add dimensions such as
environment or cell; no separate scope or service dimension is required.
Dimension names must be unique and names and values nonblank. All processes in
a pool must use the same target, overshoot, polling, bucket and skew settings;
incompatible policies never rewrite its roster or join its census. Use a new
grouping dimension or storage namespace when changing the pool policy.

Choose `if_failed` explicitly. `StartupFailure::disabled()` starts off when the
roster is unavailable, invalid, missing or unsettled. Those processes
still populate the census; only future startups can become enabled after it
settles. `StartupFailure::assume_population(n)` uses an independent draw with probability
`min(1, target / n)`. An inaccurate fallback assumption can over- or undersample.

Population discovery starts with everyone reporting. Once a census settles,
the leader chooses a hash reporting fraction `f` aimed at `32 * target` census
reporters: 480 for target 15. It estimates population as `R / f`, where `R` counts
visible census-eligible processes. No minimum population is required. Small
pools keep full reporting. Profilers outside the census subset also heartbeat
so their selection remains visible; they do not inflate `R`.

Quiet processes stop membership writes but retain jittered roster polling. They
learn an enlarged reporting fraction after the fleet shrinks and can attempt
leadership if every former reporter disappears. Changing the fraction preserves
existing profiling choices and weights. Census ranks are stable within a reporting
epoch. Each invocation gets a fresh process run identity; callers do not supply
physical host identities. Each reporting epoch uses its
fresh ID as a census hash salt after its fraction is chosen, avoiding feedback
from the same ranks used to choose the threshold.
Simultaneously live containers therefore have independent ranks. Admission and thinning
use separate fresh invocation randomness. Fresh membership IDs isolate restart
cleanup; overlapping processes count separately.

## Census, trimming and storage

Reporters write empty objects in five-minute buckets; all processes poll once
per minute, with jitter. One leader lists the current and previous buckets with
complete, unordered pagination. Carrier state wins deduplication. Failed requests prevent
publishing partial counts. Leadership and the roster share one CAS object; fresh
revisions fence paused publishers and uncertain writes require readback.

Reporting changes use an active epoch and at most one pending epoch with
separate membership keys. Existing reporters continue the active census during
collection; newly eligible processes report the pending epoch as soon as they
learn it. Promotion waits for a collection delay and four complete plateau
counts. Admission keeps the previous usable population estimate during this
handoff. Old records never enter a count divided by the new fraction. The delay
covers five polling intervals, twice the clock skew and timestamp uncertainty.
These settling checks cannot certify absent participants.

After three settled complete counts outside half to twice the reporter budget,
the leader proposes a new fraction. A complete zero-reporter census starts full
reporting recovery; it does not establish an empty fleet. A stalled non-full
handoff is replaced by full reporting while the previous admission estimate
remains unchanged. Admission estimates expire after two bucket widths, six
polling intervals and twice the clock skew,
then startup uses the configured fallback.

Four counts with at most 2% consecutive growth establish an admission cutoff.
This heuristic does not certify a complete census. Empty counts cannot establish
one. The last settled admission cutoff survives outages within its freshness
budget; stale population data can therefore over- or undersample. Existing off
processes never turn on.

A fresh census whose admitted count `A` exceeds `target + max_overshoot`
publishes a fresh thinning round with survival probability `q = target / A`.
The overshoot default is `ceil(3 * sqrt(target))`. Each eligible process applies
a round once using independent randomness; survivors increase their expansion
weight by `1/q`, and failed draws disable recording. A fully applied round on
`A` members has expected remaining count `target`, not an exact cap. Slow
polling, changing membership and partial application add separate error.
The round remains stable during a grace period; a healthy census clears it,
and new joiners skip rounds published before their startup observation.
Trimmed processes continue membership when census eligibility requires it;
the sampler never re-enables them.
Lease age uses server modification time; trimming
also checks the embedded census age. Partitions can exceed the cap indefinitely.
Bucket-edge LIST guards require `max_clock_skew` (default 5s) to bound each host
relative to storage UTC, with a safe window after backend timestamp uncertainty.
Crashes or failed cleanup can leave membership visible for two bucket widths.

S3 coordination lives under
`{configured_prefix}/_dial9_sampling/service={sha256(service_name)}/`, omitting
the configured prefix when absent. An explicit config region avoids discovery;
otherwise initialization uses `HeadBucket`, including redirect region headers.
Supply a custom `.client(...)` to the store builder inside `with_storage` for
custom credentials or endpoints. Trace `key_fn`, instance path and boot identity
do not change coordination keys.

Grant GET, PUT, LIST and membership DELETE. Within that namespace expire
**only** `v3/members/`, with retention comfortably exceeding two bucket widths
and skew (for example one day). Preserve `v3/pools/` rosters. Retain `v3/census/`
observations at least as long as the traces to be analyzed.
Custom backends implement `backend::SamplingStore` with strong reads, atomic
conditional writes, complete pagination and declared timestamp uncertainty;
unknown writes must never be hidden by blind retries.

## Historical population

The counted unit is a live process/container that could emit Dial9 traces,
including processes whose profiling choice is off. Each complete, guarded
census writes an immutable observation before attempting the roster CAS.
Membership cleanup and roster replacement do not delete observations. A
publisher that later loses its CAS still supplied a complete measurement;
the observation does not imply that its cutoffs were adopted.

```rust,ignore
let estimate = sampling.population_at()
    .store(&store)
    .at(time_slice)
    .max_age(Duration::from_secs(90))
    .call()
    .await?;
```

This returns the latest census completed before the requested time, or `None`
when no sufficiently recent evidence exists. The result includes the listing
interval, observation age, bucket width, skew, census reporter count `R`, the
observation's reporting epoch and fraction `f`, and projected population `R / f`.
For a fixed fraction independent of the measured hashes and independent identities,
the plug-in Gaussian standard error is `sqrt(R * (1-f)) / f`; sparse counts
below 30 omit that approximation. A full census has no hash sampling error.
Adaptive fraction changes and reporting delays need separate validation; this
approximation does not bound errors from the controller or handoff.

This uncertainty describes **visible membership**, not an instantaneous live
count. A short-lived process may finish between observations; a crash or
failed cleanup can leave a ghost for nearly two bucket widths. Missing
participants, clock violations, identity reuse and partitions cause separate
bias. Do not extrapolate a nearby observation into an exact time-slice count.
The admitted count describes the sampler choice; trace or recorder evidence
establishes which processes actually recorded.

## Fleet flamegraph weights

`status.register_recorder(...)` also attaches the current expansion weight.
The existing background roster refresh updates the shared weight when the process
applies a new thinning round; the source snapshots it during metadata evaluation. Decoding
needs one expansion factor for each sampling interval, rather than the full
roster or an external join.
Let `p` be the actual startup admission probability. Profiling is independent of
census eligibility. A selected process starts with weight `1 / p`;
each independent thinning round it actually survives multiplies this by `1/q`.
Missed rounds add no factor. These sequential conditional factors preserve the
expected contribution of each process despite changing cohorts and asynchronous
application; they are not its exact marginal inclusion probability.

The sampler uses separate entropy for reporting, admission and each thinning
round. Protocol schema 4 and the `v3/` namespace isolate the earlier immutable
reporting protocol. Process runs use fresh identities with separate hash domains
for census reporting, admission and thinning draws.

The source emits `dial9.sampling.weight` and `dial9.sampling.pool`. The latter
hashes sorted extra dimensions and is interpreted within the configured storage
namespace; existing trace metadata supplies service, bucket and prefix. It remains
known even when storage initialization fails. The source evaluates
the weight when the recorder polls source metadata, and the writer carries the
latest snapshot into trace segments. Unchanged weights produce no metadata
updates. The trace's existing `boot_id` supplies process identity and its clock
synchronization supplies timestamps.

Multiply a segment's sample mass by its weight, then sum by stack. The value is
correct when metadata is evaluated; applying it across the segment approximates
any weight changes within that segment. `null` or missing weights are unknown.
Zero means the sampler stopped profiling when the weight was evaluated.
An idle recorder may emit no segment, so trace presence alone is not a census.

The sampling factors preserve expected contributions under positive sampling
support, independent identities/randomness, exogenous lifetimes/workload and
usable trace delivery.
For complete startup-cohort coverage choose a positive `AssumePopulation`
fallback. A cold `Disabled` cohort has zero inclusion probability and cannot
be recovered from later traces; `no_support` in census observations exposes
census-visible members of these cohorts. Missing or dropped trace spans cannot be
repaired by census data. Workload estimates can have large variance; normalizing
expanded totals into percentages introduces the usual finite-sample ratio error.
This PR records the evidence needed by a future weighted flamegraph reader;
the existing viewer does not yet apply these factors.
