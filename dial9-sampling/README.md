# Startup sampling for dial9

`dial9-sampling` enables large fleets to automatically sample based on a set of dimensions. You can configure
a set of dimensions like regions, instance types, etc. and set a target number of profiled hosts per dimension.

A storage backend (by default, the same S3 bucket used for trace data) coordinates startup and probabilistically
controls the number of hosts enabled for profiling.

When enabled, the trace files will include metadata to help "unsample" the profiling information so that
an accurate whole-fleet profile can be created.

```rust,no_run
# #[cfg(feature = "s3")]
# fn example() -> std::io::Result<()> {
# use dial9::{Dial9HandleTokioExt, MemoryBuffer, TokioAttachOptions};
# use dial9_destinations_s3::S3Config;
# use dial9_sampling::{Sampling, SamplingStatus, StartupFailure, s3::S3SamplingStore};
# use std::{io, time::Duration};
#
# fn configure_application(
#     enabled: bool,
#     _s3: &S3Config,
# ) -> io::Result<(dial9::Recorder, tokio::runtime::Runtime)> {
#     let recorder = if enabled {
#         dial9::recorder(MemoryBuffer::new(16 * 1024 * 1024)?).build()
#     } else {
#         dial9::recorder_disabled()
#     };
#     let mut builder = tokio::runtime::Builder::new_multi_thread();
#     builder.enable_all();
#     let runtime = recorder.handle().attach_tokio_runtime(
#         builder,
#         TokioAttachOptions::default(),
#     )?;
#     Ok((recorder, runtime))
# }
#
let s3 = S3Config::builder()
    .bucket("dial9-traces-my-service")
    .service_name("checkout")
    .build();

Sampling::builder()
    .add_dimension("environment", "production")
    .add_dimension("instance-type", "m7i.large")
    .add_dimension("az", "us-east-1a")
    // aim to have 4 nodes reporting profiling information
    .num_target_profilers(4)
    // if S3 is unavailable, disable profiling
    .if_failed(StartupFailure::disabled())
    .build()
    .with_storage(async {
        S3SamplingStore::builder().config(s3.clone()).build().await
    })
    .start_application(|sampler| {
        let SamplingStatus { enabled, .. } = sampler;
        let (recorder, runtime) = configure_application(enabled, &s3)?;

        sampler.register_recorder(&recorder);
        // Run main, drop the application runtime, and drain the recorder here.
#       drop(runtime);
#       recorder.graceful_shutdown(Duration::from_secs(5));
        Ok(())
    })
# }
# fn main() {}
```

For more details see the [complete runnable example](https://github.com/dial9-rs/dial9/blob/HEAD/dial9/examples/manual_sampling.rs).

## Configuration options

There are two key options to decide on:
1. How many profilers to run via [`num_target_profilers`]. Most services will want to run profiling in 4-20 hosts.
2. What to do if you can't decide ([`if_failed`]), for example if S3 is down or times out. If your service is always ~100 hosts, you could safely use a startup probability with [`StartupFailure::assume_population`]. If it is 100k hosts, then [`StartupFailure::disabled`] may be a more sound choice.

There are a few other options that are more relevant for a subset of services:
1. What "dimensions" to use for profiling ([`add_dimension`]). Each set of K/V dimension pairs creates a "pool" of nodes. So for example, if you were running on two different instance types and wanted to ensure you had some profiling from each, you could add `instance_type` as a dimension.
2. How long to wait before hitting the fallback ([`startup_timeout`]). The startup timeout defaults to 1000ms; if your service is not latency sensitive at startup, changing to 5000ms decreases the probability of spurious timeouts.

Storage is configured with [`S3SamplingStore::builder`], which reuses the uploader's [`S3Config`].

[`num_target_profilers`]: https://docs.rs/dial9-sampling/latest/dial9_sampling/struct.SamplingBuilder.html#method.num_target_profilers
[`if_failed`]: https://docs.rs/dial9-sampling/latest/dial9_sampling/struct.SamplingBuilder.html#method.if_failed
[`add_dimension`]: https://docs.rs/dial9-sampling/latest/dial9_sampling/struct.SamplingBuilder.html#method.add_dimension
[`startup_timeout`]: https://docs.rs/dial9-sampling/latest/dial9_sampling/struct.SamplingBuilder.html#method.startup_timeout
[`StartupFailure::assume_population`]: https://docs.rs/dial9-sampling/latest/dial9_sampling/enum.StartupFailure.html#method.assume_population
[`StartupFailure::disabled`]: https://docs.rs/dial9-sampling/latest/dial9_sampling/enum.StartupFailure.html#method.disabled
[`S3SamplingStore::builder`]: https://docs.rs/dial9-sampling/latest/dial9_sampling/s3/struct.S3SamplingStore.html#method.builder
[`S3Config`]: https://docs.rs/dial9-destinations-s3/latest/dial9_destinations_s3/struct.S3Config.html


## Implementation details

Processes read a shared roster at startup and on a fixed schedule: once a minute while profiling, and once every five minutes otherwise. Processes fall into two classes:

1. A subset of nodes are "reporting": Each minute, processes make a fresh sampling decision and report once at a randomly chosen instant. These reports estimate the mean number of live processes during that completed minute.
2. A smaller subset of nodes are "profiling": These nodes have profiling enabled. The startup decision stays fixed; the sampler may later stop an enabled profiler.

When a pool has no roster yet, the first process to create one becomes its leader and profiles, whatever `if_failed` says. This keeps a freshly deployed fleet from starting with zero profilers. Its trace weight is unknown, and trimming thins it like any other profiler. Other processes that start before the first population estimate is published use `if_failed`.

One leader updates the roster using a bounded sample of the lowest random hashes from the completed minute. Reports remain immutable after a process exits, and old windows expire through the storage lifecycle policy.

## Fleet flamegraph weights

`sampler.register_recorder(...)` enables an internal `Source` that reports the current sampling expansion weight. This
will be persisted into segment metadata so that during analysis, you can properly unsample a given trace to find its true impact.
