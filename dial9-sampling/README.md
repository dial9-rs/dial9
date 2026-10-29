# Startup sampling for dial9

`dial9-sampling` enables large fleets to automatically sample based on a set of dimensions. You can configure 
a set of dimensions like regions, instance types, etc. and set a target number of profiled hosts per dimension.

A storage backend (by default, the same S3 bucket used for trace data) coordinates startup and probabilistically 
controls the number of hosts enabled for profiling.

When enabled, the trace files will include metadata to help "unsample" the profiling information so that 
an accurate whole-fleet profile can be created.

```rust,ignore
let s3 = dial9::s3::S3Config::builder()
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
    .start_application(|status| { // TODO(agent): rename this variable to `sampler`
        let SamplingStatus { enabled, .. } = status;
        let (recorder, runtime) = configure_application(enabled, &s3)?;
        
        status.register_recorder(&recorder);
        // Run main, drop the application runtime, and drain the recorder here.
        Ok(())
    })
```

For more details see the [complete runnable example](../dial9/examples/manual_sampling.rs).

## Configuration details

TODO: HUMAN

## Implementation details

When nodes start, they upload a metadata file into S3. Nodes end up being divided into two classes:
1. A subset of nodes are "reporting": These nodes report merely that they are alive. This set of nodes is used to estimate the total population size.
2. A smaller subset of nodes are "profiling": These nodes actually have profiling enabled

Lastly, 1 node becomes the leader and coordinates a roster file that tracks how many nodes are currently active.

## Fleet flamegraph weights

`status.register_recorder(...)` enables an internal `Source` that reports the current fleet size. This 
will be persisted into segment metadata so that during analysis, you can properly unsample a given trace to find its true impact.
