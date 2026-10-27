# Startup sampling

`dial9-sampling` makes one stable startup profiling choice through pluggable
object storage. It prepares sampling on its own runtime, runs your application
outside that runtime's entered context, then attempts lease cleanup. The same
sampling runtime stays alive throughout the application.

Enable the `s3` and `dial9` features for S3 coordination and automatic recorder
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
        .exact_target(15)
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
`start_application` runs. Backend initialization and admission use one private,
uninstrumented Tokio runtime with one worker. Construct executor-dependent
resources inside the backend future so SDK initialization, credential refresh
and coordination tasks all use that runtime. Trace upload uses the recorder's
separate worker runtime and a second S3 client.

Both the initialization future and application callback can borrow local values
and need not be `Send` or `'static`. The callback's result is returned unchanged,
including borrowed values and errors. See the [application entry-point
example](../dial9/examples/manual_sampling.rs) for a complete integration.

The runner owns the live sampling session through callback return or unwinding.
The application owns its usual runtime and recorder shutdown: drop the
instrumented runtime and drain the recorder before returning, and keep those
resources local so unwinding destroys them before sampling cleanup. Do not
return live recording resources from the callback. Destructuring, dropping or
returning `SamplingStatus` never retains or releases a lease. The status is
`#[non_exhaustive]`, with a public `enabled` field; include `..` when destructuring
so fields can be added later. Its initial choice is immutable.

Runtime setup, backend initialization and admission share the startup budget,
which defaults to 1000ms. Runtime creation and blocking code cannot be interrupted;
the remaining cooperative budget is checked before polling initialization and
before admission. Label discovery and the application callback are outside that
budget. Calling `start_application` from an entered Tokio runtime panics before
initialization or the application callback runs.

After the callback finishes, including errors and unwinding, cleanup attempts
owner-checked release and membership withdrawal with a separate 500ms cooperative
budget. The sampling executor is then shut down without waiting for arbitrary
blocking tasks. Abrupt process exit, partitions and failed cleanup still rely on
lease expiry. Failed writer/runtime construction follows the same cleanup path.

Callers must explicitly choose `if_failed`; the recommended choice is disabled.
Full pools, zero targets, invalid configuration and incompatible shared policies
stay off. A fixed probability draws once on storage failure/timeout and can exceed
the target. There is no later admission, rotation, or restart of stopped profiling.

A target applies independently to each complete scope/dimension tuple. The lease
protocol uses five-minute leases, one-minute renewal, five-second clock-skew
allowance, conditional ownership fencing, reconciliation of unknown writes, and
sampled membership. Backend timestamps need the uncertainty declared by the
trait. Mixed-target rollouts share slots and the largest target bounds coordinated
ownership. Nondefault lease/skew policies require deliberate pool migration before
adopting this smaller API. Default continuity after lease loss can leave actual
profilers above the target.

Optional `.stop_on_lease_loss(true)` disables registered recorders on lease loss
or maintenance termination. Register the recorder after constructing it:

```rust,ignore
.start_application(|status| {
    let SamplingStatus { enabled, .. } = status;
    let (recorder, runtime) = configure_application(enabled)?;
    status.register_recorder(&recorder);
    // Run the application, drop its runtime and drain its recorder.
})
```

A strict stop is terminal; the sampler never re-enables the recorder. The sampler
owns the monitor on its private runtime, so the application does not spawn a task
or observe a public stop future. Registration holds a weak recording reference.
All registered recorders are disabled before lease cleanup at callback completion;
normal recorder shutdown remains the application's responsibility. An initially
disabled choice, or registration through a status from an ended invocation,
disables recording immediately. Default continuity and enabled probability
fallback keep recording until application completion. The application also owns
its sampling metrics and `DIAL9_ENABLED` master switch.

The native S3 adapter is enabled by the `s3` feature; custom stores implement
`dial9_sampling::backend::SamplingStore`. In-memory simulation storage is private
test support.
Detailed population diagnostics, lease tuning and the maintenance session are
private. The tracing startup entry preserves diagnostic reasons. Backend implementers
retain the conditional-write, pagination and error contract.

The S3 prefix defaults to `_dial9_sampling/`; `.prefix(...)` overrides it verbatim.
Existing pools keep their configured prefix. If a pool used the old empty default,
set `.prefix("")` explicitly to keep using that pool. Changing the prefix creates
a separate pool.

Use the coordination prefix with existing days-based expiration. Live
policy, leases and membership are refreshed; abandoned keys age out. Retention
must comfortably exceed the fixed lease and refresh settings, so fourteen-day
trace retention can also cover coordination. Lease deadlines determine slot reuse.
Configure bucket lifecycle and IAM independently of the sampling client.

Tests cover lazy initialization, caller and sampling runtime separation, borrowed
non-Send factories/callbacks, public status destructuring, status-independent
maintenance and cleanup on success/errors/unwinding. Paused-time admission tests
cover deadline composition, fallback and automatic recorder stops after status
drop. Shuttle checks one-shot stop callbacks and registration racing cleanup.
Protocol simulations,
S3 error/timestamp regressions, 100k fleet and estimator qualification, and Shuttle
scenarios remain private unit tests. The integration regression uses separate SDK
clients against local S3 and decodes an uploaded trace marker.
