# Startup sampling

`dial9-sampling` makes one profiling decision at application startup from a
shared S3 population roster. An elected process maintains the census in the
background; excess profilers can stop permanently. The target is an expected
count. Overshoot triggers randomized thinning; the profiler count has
statistical error while storage and the census are healthy.

Enable `s3` for the native backend and `dial9` for recorder registration.

```rust,ignore
Sampling::builder()
    .scope("checkout/production")
    .add_dimension("instance-type", "m7i.large")
    .add_dimension("az", "us-east-1a")
    .target(15)
    .if_failed(StartupFailure::disabled())
    .build()
    .with_backend(async {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Ok(S3SamplingStore::builder()
            .client(aws_sdk_s3::Client::new(&config))
            .bucket("trace-bucket")
            .build())
    })
    .start_application(|status| {
        let SamplingStatus { enabled, .. } = status;
        let (recorder, runtime) = configure_application(enabled)?;
        status.register_recorder(&recorder);
        // Run main, drop the application runtime, and drain the recorder here.
        Ok(())
    })
```

See the [complete runnable example](../dial9/examples/manual_sampling.rs) for
recorder construction, runtime attachment, trace uploading and shutdown.

## Application ownership

Call `start_application` from ordinary synchronous `main`, outside an entered
Tokio runtime. Backend initialization and maintenance use one private Tokio
worker, owned until the callback returns or unwinds. Construct SDK clients inside
the backend future; the application constructs its own instrumented runtime.
Futures and callbacks may borrow local values and need not be `Send` or `'static`.
Dropping unused startup configuration performs no I/O.

The default 1000ms cooperative startup budget covers runtime setup, backend
initialization and one roster GET. Blocking code cannot be interrupted. Cleanup
has a separate 500ms budget. A zero target, invalid configuration or untracked
rank creates no backend or runtime. Dropping or returning `SamplingStatus` does
not retain or end coordination; `enabled` always describes the startup choice.

Register each recorder immediately after construction. The sampler holds a weak
recording reference and stops registered recorders after a terminal trim or
callback completion, including unwinding. Storage errors preserve recording;
the sampler never re-enables it. The application owns normal recorder shutdown.

## Population and failure policy

A target applies independently to each scope and complete dimension tuple.
Dimension names must be unique and names and values nonblank. All processes in
a pool must use the same target, tracking, overshoot, polling, bucket and skew
settings; incompatible policies never rewrite its roster or join its census.
Use a new scope when changing the pool policy.

Choose `if_failed` explicitly. `StartupFailure::disabled()` starts off when the
roster is unavailable, invalid, missing or unsettled. Those tracked processes
still populate the census; only future startups can become enabled after it
settles. `StartupFailure::assume_population(n)` uses an independent draw with probability
`min(1, target / n)`, capped by the tracking fraction. An inaccurate assumption
can over- or undersample.

By default all processes are tracked. `minimum_population(n)` asserts a lower
bound for this particular pool and chooses the largest `k` with
`n / 2^k >= 32 * target`; only ranks in the lowest `1 / 2^k` slice perform I/O.
An overstated lower bound loses small-pool coverage. A maximum fleet size cannot
justify it. A supplied `host_id` keeps the **census** rank stable across restarts;
otherwise each invocation gets a fresh identity. Simultaneously live processes
need independent identities for the statistical bounds. Profiling admission
uses separate, fresh invocation randomness; each startup choice stays fixed.
Fresh membership IDs isolate restart cleanup; overlapping processes
count separately.

## Census, trimming and storage

Members write empty objects in five-minute buckets and poll once per minute,
with jitter. One leader lists the current and previous buckets with complete,
unordered pagination. Carrier state wins deduplication. Failed requests prevent
publishing partial counts. Leadership and the roster share one CAS object; fresh
revisions fence paused publishers and uncertain writes require readback.

Four counts with at most 2% consecutive growth establish an admission cutoff.
This heuristic does not certify a complete census. Empty counts cannot establish
one. The last settled admission cutoff survives outages; stale population data
can therefore over- or undersample. Existing off processes never turn on.

A fresh census whose admitted count `A` exceeds `target + max_overshoot`
publishes a fresh thinning round with survival probability `q = target / A`.
The overshoot default is `ceil(3 * sqrt(target))`. Each eligible process applies
a round once using independent randomness; survivors increase their expansion
weight by `1/q`, and failed draws stop permanently. A fully applied round on
`A` members has expected remaining count `target`, not an exact cap. Slow
polling, changing membership and partial application add separate error.
The round remains stable during a grace period; a healthy census clears it,
and new joiners skip rounds published before their startup observation.
Trimmed carriers continue census membership and never restart recording.
Lease age uses server modification time; trimming
also checks the embedded census age. Partitions can exceed the cap indefinitely.
Bucket-edge LIST guards require `max_clock_skew` (default 5s) to bound each host
relative to storage UTC, with a safe window after backend timestamp uncertainty.
Crashes or failed cleanup can leave membership visible for two bucket widths.

The S3 prefix defaults to `_dial9_sampling/`. Grant GET, PUT, LIST and membership
DELETE. Expire **only** `v2/members/`, with retention comfortably exceeding two
bucket widths and skew (for example one day). Preserve `v2/pools/` rosters.
Retain `v2/census/` observations at least as long as the traces to be analyzed.
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
interval, observation age, bucket width, skew, tracked count `R`, tracking
fraction `f`, and projected population `R / f`. For independent identities,
the plug-in Gaussian standard error is `sqrt(R * (1-f)) / f`; sparse counts
below 30 omit that approximation. A full census has no hash sampling error.

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
Let `f` be the tracking fraction and `r` the startup admission probability
conditional on tracking. A selected process starts with weight `1 / (f*r)`;
each independent thinning round it actually survives multiplies this by `1/q`.
Missed rounds add no factor. These sequential conditional factors preserve the
expected contribution of each process despite changing cohorts and asynchronous
application; they are not its exact marginal inclusion probability.

The sampler uses separate entropy for tracking, admission and each thinning
round. Protocol schema 3 rejects peers using the earlier rank-based trimming
semantics. Configured `host_id` affects census tracking, not the fresh admission
or thinning draws.

The source emits `dial9.sampling.weight` and `dial9.sampling.pool`. It evaluates
the weight when the recorder polls source metadata, and the writer carries the
latest snapshot into trace segments. Unchanged weights produce no metadata
updates. The trace's existing `boot_id` supplies process identity and its clock
synchronization supplies timestamps.

Multiply a segment's sample mass by its weight, then sum by stack. The value is
correct when metadata is evaluated; applying it across the segment approximates
any weight changes within that segment. `null` or missing weights are unknown.
Zero means recording was permanently stopped when the weight was evaluated.
An idle recorder may emit no segment, so trace presence alone is not a census.

The sampling factors preserve expected contributions under positive sampling support, independent
identities/randomness, exogenous lifetimes/workload and usable trace delivery.
For complete startup-cohort coverage choose a positive `AssumePopulation`
fallback. A cold `Disabled` cohort has zero inclusion probability and cannot
be recovered from later traces; `no_support` in census observations exposes
tracked members of these cohorts. Missing or dropped trace spans cannot be
repaired by census data. Workload estimates can have large variance; normalizing
expanded totals into percentages introduces the usual finite-sample ratio error.
This PR records the evidence needed by a future weighted flamegraph reader;
the existing viewer does not yet apply these factors.
