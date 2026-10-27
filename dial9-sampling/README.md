# Startup sampling

`dial9-sampling` chooses profiling once at application startup from a shared
population roster. Each tracked process reads the roster once; an elected
process maintains it by listing membership in the background. The target is an
expected profiler count, with eventual trimming of excess active processes.

Enable `s3` for the native storage adapter and `dial9` for automatic recorder
registration. Without `dial9`, the sampler has no production recorder dependency.

```rust,ignore
use dial9::{Dial9HandleTokioExt, MemoryBuffer, RecorderS3ClientExt, TokioAttachOptions};
use dial9_sampling::{Sampling, SamplingStatus, StartupFailure, s3::S3SamplingStore};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let bucket = "trace-bucket";
    Sampling::builder()
        .scope("checkout/production")
        .add_dimension("instance-type", "m7i.large")
        .add_dimension("az", "us-east-1a")
        .target(15)
        .startup_timeout(Duration::from_secs(5))
        .if_failed(StartupFailure::disabled())
        .build()
        .with_backend(async {
            let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            Ok(S3SamplingStore::builder()
                .client(aws_sdk_s3::Client::new(&config))
                .bucket(bucket)
                .build())
        })
        .start_application(|status| {
            let SamplingStatus { enabled, .. } = status;
            let recorder = if enabled {
                let trace_config = dial9::s3::S3Config::builder()
                    .bucket(bucket)
                    .service_name("checkout")
                    .prefix("dial9-traces")
                    .build();
                dial9::recorder(MemoryBuffer::new(16 * 1024 * 1024)?)
                    .with_s3_uploader_client_future(trace_config, async {
                        let config =
                            aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
                        aws_sdk_s3::Client::new(&config)
                    })
                    .build()
            } else {
                dial9::recorder_disabled()
            };
            status.register_recorder(&recorder);
            // Attach recorder hooks before constructing the application runtime.
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.enable_all();
            let runtime = recorder
                .handle()
                .attach_tokio_runtime(builder, TokioAttachOptions::default())?;
            dial9::block_on(&runtime, async {
                dial9::spawn(async {
                    // Run the application's async work here.
                })
                .await
                .expect("application task");
            });
            drop(runtime);
            recorder.graceful_shutdown(Duration::from_secs(5));
            Ok(())
        })
}
```

Call this synchronous entry point from ordinary `main`, before entering Tokio.
`with_backend` is lazy: it polls nothing and creates no runtime until
`start_application` runs. Backend initialization and maintenance use one private,
uninstrumented Tokio runtime with one worker. Construct executor-dependent
resources inside the backend future so SDK initialization, credential refresh,
and coordination tasks use that runtime. Trace upload uses the recorder's
separate worker runtime and a second S3 client.

The initialization future and application callback may borrow local values and
need not be `Send` or `'static`. The callback's result is returned unchanged,
including borrowed values and errors. See the [application entry-point
example](../dial9/examples/manual_sampling.rs) for a complete integration.

The runner owns the sampling session through callback return or unwinding.
Drop the application's instrumented runtime and drain its recorder before
returning; keep those resources local so unwinding destroys them before sampling
cleanup. Destructuring, dropping, or returning `SamplingStatus` does not retain
or end coordination. The status is `#[non_exhaustive]` with a public `enabled`
field; include `..` when destructuring. Its startup choice is immutable.

Runtime setup, backend initialization, and the single startup roster GET share
the 1000ms default budget. Runtime creation and blocking code cannot be
interrupted; the remaining cooperative budget is checked before polling backend
initialization and before the GET. Label discovery and the application callback
are outside that budget. Calling `start_application` from an entered Tokio
runtime panics before initialization or the callback runs. A zero target,
untracked rank, or invalid configuration runs the callback disabled without
constructing the sampling runtime or polling the backend.

## Startup choice and population assumptions

A target applies independently to each complete scope/dimension tuple. Dimension
names must be unique and names and values nonblank. `.target(15)` requests an
expected count of 15; `.exact_target(15)` is a compatibility spelling with the
same probabilistic semantics.

Callers must explicitly choose `if_failed`. `StartupFailure::disabled()` keeps
profiling off when the roster is missing, unsettled, incompatible, malformed,
unavailable, or the startup budget expires. Disabled tracked processes still
participate in the background census. A cold fleet using this policy initially
profiles nobody; after the roster settles, future startups can use its cutoff.
Existing disabled processes never enable later.

`StartupFailure::assume_population(n)` uses the process's rank with probability
`min(1, target / n)`, restricted to the tracked membership slice. This can bootstrap
a new pool if the caller has a reasonable population estimate. A wrong estimate
can cause under- or oversampling. The compatibility constructor
`enable_with_fixed_probability(p)` uses one independent draw on startup failure,
scaled within the tracked slice so the fleet-wide probability is `min(p, f)`.
Neither fallback imposes an unconditional volume bound.

By default every process is tracked. An optional `.minimum_population(n)` is a
conservative **lower bound**, used to reduce membership traffic: the sampler
chooses the largest `k` for which `n / 2^k >= 32 * target`, and tracks ranks below
`2^(64-k)`. Untracked processes remain off and perform no storage I/O. Claiming a
lower bound above the real population sacrifices small-pool coverage. A maximum
fleet size cannot justify this assumption. The tracked fraction is fixed for the
pool's policy; choose a new scope for a different policy.

Without `.host_id(...)`, each application invocation receives a fresh rank
identity. Supplying a stable host identity makes the rank sticky across restarts.
Use distinct identities for simultaneously live processes: sharing an identity
correlates their choices, so independent-rank error bounds no longer apply.
Membership always uses a fresh invocation incarnation, allowing old and restarted
processes to overlap without cleanup deleting each other's records. Live process
incarnations are counted separately.

## Roster maintenance and permanent trimming

Each tracked process asynchronously writes empty membership objects in five-minute
time buckets and polls the roster once per minute, with jitter. Only the elected
leader lists membership, reading the current and previous buckets with complete
pagination. Member bodies are never fetched. Duplicate bucket records are counted
once per incarnation; carrier state wins within a bucket. A crashed incarnation
can remain visible for up to two bucket widths plus clock-skew allowance.

The leader stores its lease and published roster in one conditional-write object.
Every mutation changes its revision nonce. A successful takeover changes the
version used by a paused old leader, fencing that leader's later publication.
There is no separate lease/roster race and no search for profiling slots. A failed
or incomplete census does not publish partial counts. Unknown write outcomes
lose leadership and are reconciled by reading the roster.
Contenders wait for rank-based backoff and re-read the roster before attempting
one CAS; a loser waits for the next poll rather than probing repeatedly.

The leader skips census LISTs near bucket boundaries, using a margin of twice
the configured clock skew plus backend timestamp uncertainty. It also discards
a pass crossing the bucket boundary. An empty census cannot establish a usable
admission cutoff. These guards prevent bucket-skew gaps from arming a whole fleet.
`max_clock_skew` must bound each host's wall clock relative to storage UTC.
A safe census window requires `2 * (2 * max_clock_skew + backend uncertainty)`
to be smaller than the bucket width. An incompatible custom timestamp precision
is logged and leaves a new census unsettled; it cannot supply a fresh cutoff.

After four population readings with at most 2% consecutive growth, the leader
publishes an admission threshold based on `target / estimated_population`.
Convergence is a heuristic; it does not prove the census is complete. A previously
settled, compatible cutoff remains usable during growth and even after a long
outage. This preserves startup continuity, but a stale cutoff does not bound
current fleet volume. The roster also publishes a keep boundary when active
membership exceeds `target + max_overshoot`. The default overshoot is
`ceil(3 * sqrt(target))`; `.max_overshoot(...)` can override it.

Register each recorder immediately after constructing it:

```rust,ignore
.start_application(|status| {
    let SamplingStatus { enabled, .. } = status;
    let (recorder, runtime) = configure_application(enabled)?;
    status.register_recorder(&recorder);
    // Run the application, drop its runtime, and drain its recorder.
})
```

The sampler owns the stop monitor, so the application does not need to spawn a
task or observe a public stop future. A successful roster poll can permanently
trim an active process into a carrier: its recorder stops, but its membership
continues contributing to the population estimate. There is no later restart or
rotation. Storage failures preserve the current state. This is a soft bound that
requires a complete leader census and successful polling; partitions and failed
requests can delay trimming indefinitely.
Trimming requires a fresh roster, using server modification time with lease,
clock, and timestamp-precision margins. A stale keep boundary cannot trim a new
cohort merely because its old admission cutoff was reused.

Registration holds a weak recording reference and does not own recorder shutdown.
It remains effective after dropping the status. All registered recorders are
disabled before sampling cleanup when the callback returns or unwinds. An
initially disabled choice, or registration through a status from an ended
invocation, disables the recorder immediately. Fallback without a constructed
backend has no background trim source and stays enabled until callback cleanup.

After callback completion, membership withdrawal has a separate 500ms cooperative
budget. Cleanup deletes only this invocation's current and previous bucket keys;
it can require four deletes if a carrier transition left active keys behind.
The shared roster is preserved. Abrupt process exit or failed cleanup relies on
bucket aging. The sampling executor shuts down without waiting for arbitrary
blocking tasks. The application owns its metrics and any `DIAL9_ENABLED` master
switch.

## Storage policy

The native S3 prefix defaults to `_dial9_sampling/`; `.prefix(...)` overrides it
verbatim. Pools are in `v2/pools/<hash>/roster.json` and membership is in
`v2/members/<hash>/<bucket>/...`. The hash uses an unambiguous encoding of the
scope and sorted dimensions. All processes sharing a pool must use identical
target, tracked-slice, overshoot, polling, bucket, and clock-skew settings.
Incompatible policies are never rewritten; their startup follows `if_failed`
and contributes no membership to the incompatible census. If startup I/O fails,
background membership waits for a GET confirming a missing or compatible roster.
Changing the scope, dimensions, prefix, or namespace creates a separate pool.
Version 2 does not reuse the old slot-based version 1 state.

Configure lifecycle expiration for the **membership prefix only**, for example
`_dial9_sampling/v2/members/` with one-day expiration. Retention must comfortably
exceed two bucket widths and the clock allowance. Do not expire the roster prefix:
its last settled cutoff intentionally survives idle periods. IAM and bucket
lifecycle are configured separately from the sampling client.

Custom backends implement `backend::SamplingStore` with atomic conditional writes,
complete pagination, and declared modification-time uncertainty. Unordered list
pages are supported. The adapter must not blindly retry writes whose outcome is
unknown. In-memory simulation storage is private test support.

See [the protocol evidence](../docs/design/sampling/README.md) for assumptions,
mathematical bounds, finite TLA+ checks, simulation scenarios, and validation
commands. Tests preserve lazy initialization, separate caller/runtime ownership,
borrowed non-Send futures/callbacks, immutable status, terminal recorder stops,
and cleanup on success, errors, and unwinding.
