# Startup sampling

`dial9-sampling` makes one profiling decision at application startup from a
shared S3 population roster. An elected process maintains the census in the
background; excess profilers can stop permanently. The target is an expected
count, with a soft upper limit while storage and the census are healthy.

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
settles. `StartupFailure::assume_population(n)` selects by rank with probability
`min(1, target / n)`. An inaccurate assumption can over- or undersample.

By default all processes are tracked. `minimum_population(n)` asserts a lower
bound for this particular pool and chooses the largest `k` with
`n / 2^k >= 32 * target`; only ranks in the lowest `1 / 2^k` slice perform I/O.
An overstated lower bound loses small-pool coverage. A maximum fleet size cannot
justify it. A supplied `host_id` keeps the rank sticky across restarts; otherwise
each invocation gets a fresh identity. Simultaneously live processes need
independent identities for the statistical bounds. Fresh membership incarnations
isolate restart cleanup; overlapping processes count separately.

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

A fresh census can trim active processes to `target + max_overshoot`, whose
default is `ceil(3 * sqrt(target))`. Trimmed carriers continue census membership
and never restart recording. Lease age uses server modification time; trimming
also checks the embedded census age. Partitions can exceed the cap indefinitely.
Bucket-edge LIST guards require `max_clock_skew` (default 5s) to bound each host
relative to storage UTC, with a safe window after backend timestamp uncertainty.
Crashes or failed cleanup can leave membership visible for two bucket widths.

The S3 prefix defaults to `_dial9_sampling/`. Grant GET, PUT, LIST and membership
DELETE. Expire **only** `v2/members/`, with retention comfortably exceeding two
bucket widths and skew (for example one day). Preserve `v2/pools/` rosters.
Custom backends implement `backend::SamplingStore` with strong reads, atomic
conditional writes, complete pagination and declared timestamp uncertainty;
unknown writes must never be hidden by blind retries.
