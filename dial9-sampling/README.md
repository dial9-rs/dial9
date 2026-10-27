# Startup sampling

`dial9-sampling` makes a stable startup profiling choice through pluggable object
storage. It owns coordination independently of the recorder. Use the recorder's
generic `attach_object` hook to retain the session through recording shutdown.
The native S3 adapter is enabled by the `s3` feature; custom stores implement
`dial9_sampling::backend::SamplingStore`. In-memory simulation storage is opt-in.

```rust,ignore
use dial9_sampling::{Sampling, StartupFailure, s3::S3SamplingStore};
use std::time::Duration;

let sampling = Sampling::builder()
    .scope("checkout/production")
    .add_dimension("instance-type", instance_type)
    .add_dimension("az", az)
    .exact_target(15)
    .startup_timeout(Duration::from_secs(5))
    .if_failed(StartupFailure::disabled())
    .build();
let sampler = sampling.prepare(async {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    Ok(S3SamplingStore::builder()
        .client(aws_sdk_s3::Client::new(&config))
        .bucket(bucket)
        .prefix("_dial9_sampling/checkout/")
        .build())
}).await;
let recorder = if sampler.enabled() {
    dial9::recorder(MemoryBuffer::new(16 * 1024 * 1024)?)
        .with_s3_uploader_client_future(trace_config, async {
            let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            aws_sdk_s3::Client::new(&config)
        })
        .build()
} else {
    dial9::recorder_disabled()
}.attach_object(sampler);
// Attach recorder hooks, then construct and run the application runtime.
// Drop that runtime before draining the recorder.
recorder.graceful_shutdown(Duration::from_secs(5));
```

The sampler owns a private uninstrumented Tokio runtime with one worker. Its
backend future is polled in that runtime's context, so SDK initialization,
credential refresh and coordination tasks use the same executor. Construct
runtime-dependent resources inside the supplied async block; externally created
resources can still depend on their original executor. Trace upload uses the
recorder's separate worker runtime and a second S3 client.

`prepare` remains async and accepts borrowed, non-Send initialization futures.
The bootstrap executor can disappear when preparation completes. Runtime setup,
backend initialization and admission share the startup budget. Runtime creation
and blocking code cannot be interrupted; the remaining cooperative budget is
checked before admission. Label discovery and recorder/application runtime
construction are outside this budget.

See [the plain main example](../dial9/examples/manual_sampling.rs) for local
ownership, or [the macro example](../dial9/examples/sampling_smoke.rs) for
`#[dial9::main(config_async = configure)]`. Both return the ordinary
`(Recorder, Runtime)` pair. The generic configuration macro and recorder know
nothing about sampling. No static guard, separate retained configuration runtime
or explicit application shutdown callback is needed. `attach_object` accepts
any `Send + Sync + 'static` value; no application lifecycle trait is required.

The application-facing API is `Sampling`, `StartupFailure`, and `SamplingSession`
plus their generated builders. `prepare` returns the guard directly. The guard
has `enabled()`, `stopped()`, and `shutdown(timeout)`; the initial choice is immutable.
Detailed snapshots, population estimates, raw random draws, lease tuning and
intermediate preparation types are private. Debug output and a tracing startup
entry retain diagnostic reasons. Backend implementers still need the existing
conditional-write, pagination and error types; placing those under `backend`
organizes that public contract without claiming to eliminate it.

Default preparation is 500ms. Callers must explicitly choose `if_failed`; the
recommended choice is disabled. Full pools, zero targets, invalid configuration
and incompatible shared policies stay off. A fixed probability draws once on
storage failure/timeout and can exceed the target. There is no later admission,
rotation, or restart of stopped profiling.

A target applies independently to each complete scope/dimension tuple. The
lease protocol is unchanged: five-minute leases, one-minute renewal, five-second
clock-skew allowance, conditional ownership fencing, reconciliation of unknown
writes, and sampled membership. Backend timestamps need the uncertainty declared
by the trait. Mixed-target rollouts share slots and the largest target bounds
coordinated ownership. Existing nondefault lease/skew policies cannot be configured
through this smaller API; migrate those pools deliberately or retain advanced
configuration before adopting this variant. Defaults favor profiling continuity after lease loss, so
actual profilers can exceed the target.

Optional `.stop_on_lease_loss(true)` requires caller wiring. Create the owned
stop future before moving the sampler into the recorder:

```rust,ignore
let stopped = sampler.stopped(); // Owned future; does not borrow the guard.
let handle = recorder.handle().clone();
let recorder = recorder.attach_object(sampler);
application_runtime.spawn(async move {
    stopped.await;
    handle.disable();
});
```

A strict stop is terminal for the sampler; it never asks to resume. Enforcement
latency now depends on the caller's monitor and executor. Default continuity
never issues a stop. Probability fallback has no confirmed lease and also never
issues a stop. An initially disabled guard's stop future is immediately ready.

Stop actual profiling before releasing the guard. `attach_object(sampler)` keeps
coordination alive through recording finalization and, on `graceful_shutdown`,
through worker drain. Ordinary recorder drop requests worker shutdown before
dropping attached values; it does not join that worker. The instrumented
application runtime must still be dropped before the recorder finalizes, so its
worker threads can flush their events.

Sampler drop attempts owner-checked cleanup with a 500ms cooperative budget on
its own runtime. Outside a Tokio context, drop waits for that attempt. Inside a
Tokio context it schedules cleanup and returns without blocking; the private
runtime stays alive until the attempt completes or times out. Explicit
`sampler.shutdown(timeout).await` waits for the attempt in either context, and
canceling that wait does not cancel cleanup. Sampler cleanup is separate from
the recorder's worker drain timeout. Abrupt process exit, partitions and failed
cleanup still rely on lease expiry. A raw attached Tokio runtime must have a safe
destructor if its recorder can be dropped inside async code; the sampler handles
this internally.

Failed writer/runtime setup drops the sampler and triggers the same cleanup,
including unwind paths. Automatic recorder sampling metrics and
sampling-specific environment configuration helpers remain removed. The
application owns metrics and its `DIAL9_ENABLED` master switch.

Use a separate coordination prefix with existing days-based expiration. The
protocol refreshes live policy, leases and membership; abandoned keys age out.
Retention must comfortably exceed the fixed lease and refresh settings, so the
previous fourteen-day trace retention can also cover coordination. Application
lease deadlines still determine slot reuse. Configure bucket lifecycle and IAM
independently of the sampling client.

Protocol simulations, error regressions, timestamp-precision checks, estimator
qualification, 100k fleets and Shuttle scenarios remain private unit tests. The
shared admission tests use paused time for deadline composition, failure
fallback and optional stops. Public API tests cover owned-executor teardown,
async drop, canceled shutdown and initialization panic/cancellation; manual integration tests use two SDK clients against
local S3 and decode an uploaded marker. Population estimates are internal
diagnostics, not a public fleet-statistics API.
