# Migrating from 0.3 to 0.5

0.5 is a breaking release. Four things change, and the rest follows from them.

- `dial9` is now the main crate to depend on, rather than `dial9-tokio-telemetry`. The feature flags you already use (`cpu-profiling`, `memory-profiling`, `worker-s3`, ...) come with it. If you also pulled in `dial9-trace-format` to define custom events, you can drop it.
- `Dial9Config` is gone: In 0.3, one config object described the trace file, the Tokio runtime and every data source at once, and the `#[main]` macro turned it into a running runtime. 0.5 splits that in two: a **buffer** (previously named writer) which is where trace bytes go, and a **`Recorder`** you build from it, which is the thing that actually records. You hold the `Recorder` for the life of the program.
- **Tokio is optional now.** CPU, memory and rusage profiling record on their own, so a program that never touches Tokio no longer compiles it in. If you do use Tokio, you attach a runtime to the recorder rather than letting a config build one for you.
- **The handles were renamed.** `TelemetryHandle` and `RuntimeTelemetryHandle` became `Dial9Handle`, for recording and control, and `Dial9TokioHandle`, for spawning tasks.

Jump to the [rename table](#rename-table) if you just want the mechanical diff.

---

## 1. Dependencies

```toml
# 0.3
dial9-tokio-telemetry = { version = "0.3", features = ["cpu-profiling", "memory-profiling", "worker-s3"] }
dial9-trace-format = "0.4"

# 0.5
dial9 = { version = "0.5", features = ["tokio", "cpu-profiling", "memory-profiling", "worker-s3"] }
```

`dial9` ships **no default features**, so a library dependency pulls in no viewer or CLI weight. Enable what you use:

| Feature | What it gets you |
| --- | --- |
| `tokio` | Runtime instrumentation, `#[dial9::main]`, `spawn`, `Dial9TokioHandle` |
| `cpu-profiling` | CPU stack sampling and scheduler events |
| `memory-profiling` | Allocation sampling (still needs the global allocator, see #7) |
| `process-resource` | rusage sampling. **Now opt-in**, was on by default on Unix |
| `linux-socket` | TCP accept-queue depth sampling |
| `worker-s3` | S3 upload |
| `metrique-sink` | New: record metrique entries into the trace |
| `analysis` | Offline trace decode and analysis |
| `taskdump` | Async backtraces at yield points |
| `cli` | The `dial9` binary. Don't put this on a service dependency |

Three things to watch:

- **`process-resource` is opt-in now.** In 0.3 rusage events were on by default on Unix. If you relied on rss / page-fault events, add the feature or they quietly disappear.
- **`tracing-layer` moved out.** See #9.
- **Install the CLI separately:** `cargo install dial9 --features cli`.

### `--cfg tokio_unstable` is optional

0.3 required it, dial9 now builds without it, at the cost of narrower task coverage: poll events come from dial9's own future wrapper, so they cover tasks spawned through `dial9::spawn`, `spawn_in`, `spawn_with` and `block_on` rather than every task on the runtime. Task spawn/terminate events and per-worker queue depth are unavailable without it.

Traces carry `tokio.poll_coverage`, `tokio.local_queue` and `tokio.unstable` metadata so the viewer can tell you what a trace holds.

`taskdump` still needs `--cfg tokio_unstable`: it forwards to `tokio/taskdump`, which won't compile without it.

---

## 2. Config becomes a recorder builder

`Dial9Config`, `Dial9ConfigBuilder`, the legacy positional `config` module, `TracedRuntime`, `TracedRuntime::builder()` and the pipeline / trace-path type-state markers are all gone.

You now build a buffer, wrap it in `dial9::recorder(..)`, register the sources you want, and `build()`. A **source** is one producer of trace data: CPU sampling, memory profiling, rusage, Tokio itself. In 0.3 these were knobs inside `with_runtime(|r| ...)`.

```rust
// 0.3
fn my_config() -> Dial9Config {
    Dial9Config::builder()
        .base_path("/tmp/my_traces/trace.bin")
        .max_total_size(5 * 1024 * 1024)
        .with_runtime(|r| r.with_runtime_name("main").with_task_tracking(true))
        .with_tokio(|t| { t.worker_threads(4); })
        .build_or_disabled()
}
```

```rust
// 0.5
use std::io;
use dial9::{AttachedRuntime, Dial9HandleTokioExt, DiskBuffer, TokioAttachOptions};

fn my_config() -> io::Result<AttachedRuntime> {
    let writer = DiskBuffer::builder()
        .base_path("/tmp/my_traces")     // a directory now, see #3
        .max_total_size(5 * 1024 * 1024)
        .build();

    let recorder = dial9::recorder_or_disabled(writer).build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(4);
    let runtime = recorder.handle().attach_tokio_runtime(
        builder,
        TokioAttachOptions::builder()
            .runtime_name("main")
            .task_tracking_enabled(true)
            .build(),
    )?;

    Ok((recorder, runtime))
}
```

Where the 0.3 knobs went:

- **Sources and pipeline** (`with_cpu_profiling`, `with_memory_profiling`, `with_process_resource_usage`, `with_socket_accept_queues`, `with_custom_events`, `with_custom_pipeline`, `with_s3_uploader`) live on the **recorder builder**.
- **Per-runtime settings** (`runtime_name`, `task_tracking_enabled`, `tokio_instrumentation_enabled`, `task_dump_config`, `tokio_hooks`) live in **`TokioAttachOptions`**.
- **Buffer settings** (`base_path`, `max_file_size`, `max_total_size`, `rotation_period`) live on **`DiskBuffer::builder()`**.

### `build()` starts recording

`build_and_start()` is removed, because `build()` is what it did. Chain `.paused()` before it if you want a quiet recorder you start later with `Recorder::enable()`.

### Three entry points instead of `build_or_disabled()`

| You want | 0.5 |
| --- | --- |
| Surface writer errors | `dial9::recorder(writer?)` |
| Degrade to disabled if the writer fails | `dial9::recorder_or_disabled(writer)` |
| Telemetry off | `dial9::recorder_disabled()` (returns a `Recorder`, not a builder) |

`recorder_or_disabled` is the closest match to 0.3's `build_or_disabled()`: sources and a pipeline still chain onto it.

### Env-driven setup

The `DIAL9_*` variables are unchanged. The one-call entry point:

```rust
// 0.3
#[dial9_tokio_telemetry::main(config = Dial9Config::from_env)]

// 0.5
#[dial9::main(config = dial9::recorder_from_env)]
```

Or by hand, when you need the recorder and the runtime as values:

```rust
let (recorder, runtime) = dial9::recorder_from_env()?;
// or with control over the runtime it builds:
let (recorder, runtime) = dial9::recorder_from_env_with(|b| { b.worker_threads(4); })?;
```

`recorder_from_env` never fails to disabled. If `DIAL9_ENABLED` is unset or false, you get a disabled recorder plus a plain runtime. Check with `recorder.handle().is_enabled()` rather than expecting an `Err` or a `None`.

### One recorder per process

A second `build()` in the same process returns a **disabled** recorder and logs an `error!`. The memory profiler, the allocator, CPU profiling and the process-global handle are singletons and the first recorder already claimed them. Dropping the first recorder frees the slot.

---

## 3. Writers become buffers, and `base_path` is a directory

| 0.3 | 0.5 |
| --- | --- |
| `RotatingWriter` | `DiskBuffer` |
| `.base_path("/tmp/traces/trace.bin")` | `DiskBuffer::builder().base_path("/tmp/traces")…build()` |
| in-memory writer | `MemoryBuffer::new(max_total_size)` |
| `NullWriter` | `MemoryBuffer::new(small)` |
| `TraceWriter` trait | removed, `DiskBuffer` / `MemoryBuffer` are concrete |

**`base_path` changed meaning.** In 0.3 it was a *file* path and dial9 rotated around it (`/tmp/traces/trace.bin` → `trace.0.bin`, `trace.1.bin`, …). In 0.5 it is the *directory* the trace lives in. Passing a file path still "works" but produces a directory named `trace.bin`, which is almost certainly not what you want. Strip the filename.

Both constructors return `io::Result`:

```rust
let writer = DiskBuffer::builder().base_path("/tmp/traces").max_total_size(1 << 30).build()?;
let writer = MemoryBuffer::new(16 * 1024 * 1024)?;
```

`MemoryBuffer` gives you a fully in-memory pipeline with no filesystem dependency: segments stay in process memory and ship through the same processors (gzip, S3, custom). `max_total_size` bounds it, and the oldest sealed segments drop rather than blocking recording if an exporter falls behind.

---

## 4. Tokio is a source you attach

In 0.3 the config owned the runtime. In 0.5 you attach a runtime to a `Dial9Handle`, so single-runtime and multi-runtime setups are the same code.

```rust
use dial9::{Dial9HandleTokioExt, TokioAttachOptions};

let recorder = dial9::recorder(writer).build();

let mut builder = tokio::runtime::Builder::new_multi_thread();
builder.enable_all().worker_threads(4);

let runtime = recorder.handle().attach_tokio_runtime(
    builder,
    TokioAttachOptions::builder().runtime_name("api").build(),
)?;
```

Notes:

- **You build the Tokio builder yourself.** That means calling `enable_all()` and picking the flavor, both of which 0.3 did implicitly.
- Attach **borrows** the handle. Clone the handle and several threads can each attach their own runtime.
- Attaching after `graceful_shutdown` returns an error. Handles that outlive shutdown go inert: `is_enabled()` reports false, `enable()` is a no-op.
- `attach_tokio_runtime` lives on `Dial9HandleTokioExt`, so that trait must be in scope.

If you saw the rc1 shape (`Recorder::attach_tokio_runtime`, `RecorderTokioExt`), that was replaced in rc2. Attach through the handle.

### `#[dial9::main]`

The macro's `config` is any zero-argument function returning `io::Result<AttachedRuntime>`, where `AttachedRuntime` is `(Recorder, tokio::runtime::Runtime)`. It panics on `Err`.

```rust
#[dial9::main(config = dial9::recorder_from_env)]
async fn main() { /* ... */ }
```

**Behavior change:** the macro now performs an implicit graceful shutdown after the body returns. It drops the runtime and drains the worker so the final segment is symbolized, compressed and uploaded. Clean exit therefore blocks up to the deadline (default 1s):

```rust
#[dial9::main(graceful_shutdown = Duration::from_secs(5))]
#[dial9::main(disable_graceful_shutdown)]
```

### Driving the runtime yourself

Use `dial9::block_on(&runtime, fut)` rather than `Runtime::block_on`. Poll and wake events come from per-task hooks, and `Runtime::block_on` polls its future outside any task, so that future and everything awaited inline under it would be missing from the trace. `dial9::block_on` spawns it first.

#### `block_on` bounds: `Send + 'static`

Because `dial9::block_on` spawns the future as a Tokio task, both the future and its output must be `Send + 'static`:

```rust
pub fn block_on<F>(runtime: &tokio::runtime::Runtime, future: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
```

This is stricter than `Runtime::block_on`, which has no `Send` or `'static` requirement at all. In practice the future itself is usually `Send` already, but the **output type** catches people: `Result<(), Box<dyn std::error::Error>>` is *not* `Send` because `Box<dyn Error>` defaults to `Box<dyn Error + 'static>` (no `Send`). Passing it to `dial9::block_on` produces a compile error on `F::Output: Send`.

Remedies:

- Use `Box<dyn std::error::Error + Send + Sync>` (or a type alias for it) as the error type through the affected call chain.
- Use a concrete, `Send`-implementing error type such as `anyhow::Error` or a project-specific enum.
- Narrow the scope: keep `dial9::block_on` around a thin wrapper and convert non-`Send` errors at its boundary.

```rust
// Before (does not compile with dial9::block_on):
async fn run() -> Result<(), Box<dyn std::error::Error>> { /* ... */ Ok(()) }

// After — make the output Send:
async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> { /* ... */ Ok(()) }

dial9::block_on(&runtime, run());
```

### Lifecycle at a glance

For a typical service that records Tokio runtime telemetry and uploads to S3:

1. **Build the buffer** — `DiskBuffer::builder()…build()`.
2. **Build the recorder** — `dial9::recorder(writer).with_s3_uploader_client_future(…).build()`. The S3 client future is stored but not yet polled.
3. **Attach the application runtime** — `handle.attach_tokio_runtime(builder, opts)`.
4. **`dial9::block_on(&runtime, root_future)`** — the pipeline worker starts on its own thread, polls the S3 client future, and begins uploading sealed segments.
5. **`drop(runtime)`** — stops Tokio workers and flushes the final segment.
6. **`recorder.graceful_shutdown(timeout)`** — drains the pipeline (symbolize, compress, upload) within the deadline, then returns.

---

## 5. Handles

`TelemetryHandle` and `RuntimeTelemetryHandle` collapsed:

- **`Dial9Handle`**: record events, enable/pause, attach runtimes, track threads.
- **`Dial9TokioHandle`**: spawn instrumented tasks.

```rust
// 0.3
let handle = TelemetryHandle::current();
handle.spawn(async { ... });
record_event(event, &handle);

// 0.5
let handle = Dial9TokioHandle::current();
handle.spawn(async { ... });

Dial9Handle::current().record_event(event);
```

`Dial9Handle::is_enabled()` changed meaning: it now reports whether recording *currently does anything* (connected **and** not paused), not merely whether it is connected. Use it to gate expensive event construction.

`Dial9Handle::current()` resolves on threads dial9 considers tracked, which Tokio workers are automatically. For a plain `std::thread`, either install a global handle (see #6) or call `Dial9Handle::track_current_thread()` and hold the returned guard. That guard also opts the thread into per-thread sampling sources such as the scheduler-event profiler.

---

## 6. Process-global handle (new)

In 0.3 you could stash a handle in a `static OnceLock<TelemetryHandle>` so synchronous worker threads could record without threading a handle through. That is built in now:

```rust
recorder.install_global_handle()?;

// on any thread, with no handle in hand:
dial9::record_event(MyEvent { /* ... */ });
// or explicitly:
Dial9Handle::current().record_event(MyEvent { /* ... */ });
```

`Dial9Handle::current()` falls back to the global handle on threads that have none. One recorder holds the slot: a second install returns `InstallGlobalHandleError`.

---

## 7. Memory profiling

`Dial9Allocator`, `MemoryProfiler` and `MemoryProfilingConfig` moved to `dial9::memory` (backed by `dial9-perf-self-profile`). The API is unchanged.

Memory profiling is env-configurable now. With the `memory-profiling` feature and `DIAL9_MEMORY_PROFILE_ENABLED=true`, `recorder_from_env` installs the profiler for you, so the manual `MemoryProfiler::from_config(..).install(handle)` call is no longer needed on that path. Your binary still declares the allocator itself, now `use dial9::memory::Dial9Allocator;`.

New env vars: `DIAL9_MEMORY_PROFILE_ENABLED`, `DIAL9_MEMORY_SAMPLE_RATE_BYTES`, `DIAL9_MEMORY_TRACK_LIVESET`.

Building programmatically, chain it on the recorder builder:

```rust
use dial9::RecorderPerfExt;
use dial9::memory::MemoryProfilingConfig;

let recorder = dial9::recorder(writer)
    .with_memory_profiling(
        MemoryProfilingConfig::builder()
            .sample_rate_bytes(512 * 1024)
            .track_liveset(true)
            .build(),
    )
    .build();
```

---

## 8. The other sources

All of them are `.with_*` on the recorder builder now, behind `RecorderPerfExt`, instead of `with_runtime(|r| ...)`:

```rust
use dial9::RecorderPerfExt;
use dial9::cpu::{CpuProfilingConfig, SchedEventConfig};
use dial9::process::ProcessResourceUsageConfig;
use dial9::socket::SocketAcceptQueuesConfig;

let recorder = dial9::recorder(writer)
    .with_cpu_profiling(CpuProfilingConfig::default())
    .with_sched_events(SchedEventConfig::default().include_kernel(true))
    .with_process_resource_usage(ProcessResourceUsageConfig::default())
    .with_socket_accept_queues(SocketAcceptQueuesConfig::default())
    .build();
```

Module moves:

| 0.3 | 0.5 |
| --- | --- |
| `telemetry::cpu_profile::{CpuProfilingConfig, SchedEventConfig}` | `dial9::cpu::*` |
| `telemetry::ProcessResourceUsageConfig` | `dial9::process::*` |
| `memory_profiling::*` | `dial9::memory::*` |
| (new in 0.5) | `dial9::socket::*` |

CPU profiling gained explicit backend selection: `CpuProfilingConfig::with_perf_backend()` and `with_ctimer_backend()`. The default (try perf, fall back to ctimer) is unchanged.

### CPU profiling without Tokio

0.3 needed `with_tokio_instrumentation(false)` to use dial9 as a plain CPU profiler. In 0.5 there is nothing to turn off. Build a recorder, don't attach a runtime:

```rust
let recorder = dial9::recorder(writer)
    .with_cpu_profiling(CpuProfilingConfig::default())
    .build();
```

For threads that should be sampled, call `handle.track_current_thread()` and hold the guard.

### Task dumps

`TaskDumpConfig` is unchanged, but it moved into `TokioAttachOptions`:

```rust
TokioAttachOptions::builder()
    .task_tracking_enabled(true)
    .task_dump_config(TaskDumpConfig::builder().idle_threshold(Duration::from_millis(10)).build())
    .build()
```

---

## 9. The tracing layer moved crates and changed name

`dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer` is now `dial9_utils::tracing_layer::Dial9TracingLayer`. The `tracing-layer` feature moved to a separate `dial9-utils` crate that `dial9` does not re-export, so add `dial9-utils = { version = "0.5", features = ["tracing-layer"] }` alongside `dial9`.

---

## 10. Custom events

The derive is unchanged, but the imports moved and there are two behavior changes.

```rust
// 0.3
use dial9_trace_format::TraceEvent;
use dial9_tokio_telemetry::telemetry::{record_event, clock_monotonic_ns};

record_event(RequestCompleted { .. }, &handle);
```

```rust
// 0.5
use dial9::format::TraceEvent;
use dial9::core::clock_monotonic_ns;

handle.record_event(RequestCompleted { .. });
// or, with a global handle installed:
dial9::record_event(RequestCompleted { .. });
```

Two behavior changes:

- **`#[derive(TraceEvent)]` is strict now.** Unknown `#[traceevent(...)]` attributes and field roles outside the supported vocabulary are compile errors instead of being silently ignored. If you had a typo'd attribute that was quietly doing nothing, it will now fail to build.
- **Events may borrow.** `&str` and `&[u8]` fields are supported, so a hot-path event no longer has to allocate a `String`.

### Custom event callbacks (new)

Instead of threading a handle around, register a callback that runs on dial9's flush thread:

```rust
use dial9::core::CustomEventsConfig;
use dial9::RecorderSourceExt;

let recorder = dial9::recorder(writer)
    .with_custom_events(CustomEventsConfig::default(), move |ctx| {
        while let Ok(event) = rx.try_recv() {
            ctx.record_event(event);
        }
    })
    .build();
```

---

## 11. Custom Tokio hooks

`with_tokio_hooks(|hooks| ...)` on the builder became a `TokioHooks` value you pass through `TokioAttachOptions`:

```rust
use dial9::{Dial9HandleTokioExt, TokioAttachOptions, TokioHooks};

let mut hooks = TokioHooks::default();
hooks.on_thread_start(|| println!("worker started"));

let runtime = recorder.handle().attach_tokio_runtime(
    builder,
    TokioAttachOptions::builder().tokio_hooks(hooks).build(),
)?;
```

Same rules as 0.3: dial9's hooks run first, yours fire in registration order, and you must not set hooks directly on the `tokio::runtime::Builder` you hand to `attach_tokio_runtime` because dial9 overwrites them.

---

## 12. Shutdown

Keep the recorder **and** its runtime alive for the duration of profiling. On shutdown, drop the runtime first so its workers flush, then drain:

```rust
// 0.3
guard.graceful_shutdown(Duration::from_secs(5))?;   // io::Result

// 0.5
drop(runtime);
recorder.graceful_shutdown(Duration::from_secs(5)); // consumes the recorder, returns ()
```

`graceful_shutdown` takes the recorder by value and returns `()` instead of an always-`Ok` `io::Result`. Drain failures are logged, as they already were.

`#[dial9::main]` does both steps for you, bounded by the graceful-shutdown deadline (see #4).

---

## 13. S3

`S3Config` moved and the default key layout changed.

```rust
// 0.3
use dial9_tokio_telemetry::background_task::s3::S3Config;
Dial9Config::builder().with_runtime(|r| r.with_s3_uploader(s3_config))

// 0.5
use dial9::s3::S3Config;
use dial9::RecorderPipelineExt;
dial9::recorder(writer).with_s3_uploader(s3_config)
```

`boot_id` is no longer an `S3Config` builder field. The runtime injects the on-disk namespace `boot_id` at build time so a local segment and its S3 key share one identity. An `S3Config` built outside the managed `recorder_from_env` path falls back to a fresh `{4-alpha}-{pid}`.

The uploader may now be configured before or after the sources whose data it symbolizes.

### Key layout is Hive-partitioned now

```
<prefix>/version=1/date=<YYYY-MM-DD>/service=<name>/time=<HH-MM>/instance=<host>/boot=<boot_id>/<epoch>-<seq>.bin.gz
```

The viewer reads both this and the historical layout, and custom `S3KeyFn` output is unchanged. If you have tooling that lists or downloads segments by key, update it to filter on the `service=` / `date=` partitions.

### Async client construction

For custom credentials or endpoints, defer client construction to the pipeline worker runtime with `with_s3_uploader_client_future(config, fut)`. The future is polled when the worker starts, and the client (including credential refresh) stays on that runtime.

The trait is `dial9::RecorderS3ClientExt`. The future must be `Send + 'static` and must resolve to `aws_sdk_s3::Client` directly — not `Result<Client, E>`. Handle credential or config initialization errors inside the future; a panic will take down the pipeline worker thread.

```rust
use dial9::RecorderS3ClientExt;
use dial9::s3::S3Config;

let s3_config = S3Config::builder()
    .bucket("my-bucket")
    .service_name("my-service")
    .region("us-east-1")
    .build();

let recorder = dial9::recorder(writer)
    .with_s3_uploader_client_future(s3_config, async {
        // Credential/config errors must be handled here, not propagated.
        // If initialization fails, degrade to a dummy client or log and
        // disable telemetry rather than crashing the application.
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        aws_sdk_s3::Client::new(&config)
    })
    .build();
```

A good default policy for the error branch is to log the failure and return a client that will fail uploads (the worker retries and eventually drops segments), effectively degrading telemetry instead of taking down the process.

---

## 14. Trace format and decode changes

Only relevant if you read traces yourself. The **wire format is unchanged** and old traces still decode.

- **Optional timestamps are gone from the API.** `SchemaEntry::new` and `SchemaEntry::with_annotations` no longer take a `has_timestamp` argument, and `timestamp_ns` on `DecodedFrame::Event`, `DecodedFrameRef::Event` and `RawEvent` is `u64` instead of `Option<u64>`. Legacy `has_timestamp=0` schemas still decode.
- **`SegmentData::segment()` returns `&SegmentRef`.**
- **`QueueSampleEvent` -> `RuntimeMetricsEvent`.** Per-runtime scheduler metrics replaced the process-wide queue sample: one sample per runtime per flush cycle, tagged with the runtime name. `QueueSampleEvent` still decodes, and the viewer falls back to it for traces without runtime metrics.
- **`analysis_unstable` -> `dial9::analysis`,** behind the `analysis` feature.
- Custom processors gained `SegmentProcessor::finalize_dump` and `ProcessError::into_parts`.

---

## 15. Writing a `Source` or a processor

Relevant if you wish to implement dial9's extension traits:

- `Source` now has an `Any` supertrait, so a source must be `'static`. Boxed sources already were.
- `Source::on_worker_thread_start` is renamed to `Source::on_thread_start`. It fires whenever a thread joins the recorder, which is no longer only a Tokio worker's first poll.
- `Source::segment_processor` lets a source contribute a stage to the default pipeline. The CPU profiler uses it to pull in symbolization, so a trace with stack samples is symbolized without the caller wiring anything up.
- `ThreadLocalEncoder::write_event` returns `io::Result<()>` instead of panicking the calling thread on a validation failure. The event is dropped and you own reporting.

---

## 16. Viewer S3 feature

`dial9-viewer` exposes its S3 APIs behind a default-on `s3` feature. The `dial9` crate keeps its empty default feature set, and `cli` still gives you the S3-enabled binary. For a local-only viewer with no AWS SDK, depend on `dial9-viewer` directly with default features disabled.

---

## New in 0.5, no 0.3 equivalent

Worth knowing about while you're in here:

- **Metrique sink** (`metrique-sink`): record metrique unit-of-work entries into the trace as a peer of your EMF/JSON pipeline, with per-request thread, task and timing context.
- **Ad-hoc spans** (`dial9-utils`, `span` / `tower` features): a sync guard, a future wrapper, a tower layer, and `dial9_span!` for compile-time span schemas.
- **Axum integration** (`dial9-utils`, `axum-07` / `axum-08`): traced replacements for `axum::serve`.
- **`JoinSetExt`**: `spawn_traced` / `spawn_traced_on` on Tokio `JoinSet`, keeping caller locations.
- **`dial9::spawn_in(&runtime, fut)`**: spawn an instrumented task onto a specific runtime from any thread.
- **Socket accept-queue source** (`linux-socket`).
- **In-memory pipeline**: `MemoryBuffer` with no filesystem dependency at all.
- **`segment_metadata`**: static context (service, host, deployment, version) attached to every rotated segment so it can be read independently.

---

## Rename table

| 0.3 | 0.5 |
| --- | --- |
| `dial9-tokio-telemetry` dependency | `dial9` with the `tokio` feature |
| `dial9-trace-format` dependency | not needed, use `dial9::format::*` |
| `#[dial9_tokio_telemetry::main]` | `#[dial9::main]` |
| `Dial9Config::builder()…build()` | `dial9::recorder(writer)…build()` |
| `Dial9Config::from_env()` | `dial9::recorder_from_env()` → `io::Result<AttachedRuntime>` |
| `.build_or_disabled()` | `dial9::recorder_or_disabled(writer)…build()` |
| `.build_and_start()` | `.build()` (`.paused()` to opt out) |
| `RotatingWriter` | `DiskBuffer` |
| `.base_path("dir/trace.bin")` | `DiskBuffer::builder().base_path("dir")` (directory) |
| `NullWriter` | `MemoryBuffer::new(small)?` |
| `TraceWriter` trait | removed |
| `TracedRuntime`, `TracedRuntime::builder()` | `handle.attach_tokio_runtime(builder, opts)` |
| `.with_runtime(\|r\| ...)` | sources on the recorder builder, runtime knobs in `TokioAttachOptions` |
| `.with_tokio(\|t\| ...)` | build the `tokio::runtime::Builder` yourself |
| `.with_tokio_hooks(\|h\| ...)` | `TokioAttachOptions::builder().tokio_hooks(hooks)` |
| `.with_tokio_instrumentation(false)` | don't attach a runtime |
| `TelemetryHandle` / `RuntimeTelemetryHandle` | `Dial9Handle` |
| `TelemetryHandle::current().spawn(..)` | `Dial9TokioHandle::current().spawn(..)` |
| `TelemetryGuard::graceful_shutdown(t)?` | `recorder.graceful_shutdown(t)` (returns `()`) |
| `record_event(event, &handle)` | `handle.record_event(event)` or `dial9::record_event(event)` |
| `telemetry::clock_monotonic_ns()` | `dial9::core::clock_monotonic_ns()` |
| `telemetry::cpu_profile::*` | `dial9::cpu::*` |
| `telemetry::ProcessResourceUsageConfig` | `dial9::process::*` |
| `memory_profiling::Dial9Allocator` | `dial9::memory::Dial9Allocator` |
| `background_task::s3::S3Config` | `dial9::s3::S3Config` |
| `tracing_layer::Dial9TokioLayer` | `dial9_utils::tracing_layer::Dial9TracingLayer` |
| `analysis_unstable` | `dial9::analysis` (feature `analysis`) |
| `QueueSampleEvent` | `RuntimeMetricsEvent` (old one still decodes) |
| `Source::on_worker_thread_start` | `Source::on_thread_start` |
| rusage on by default on Unix | opt-in `process-resource` feature |
| `--cfg tokio_unstable` required | optional, reduced task coverage without it |
| static `OnceLock<TelemetryHandle>` | `recorder.install_global_handle()` + `Dial9Handle::current()` |

---

## Env variables

Every `DIAL9_*` variable from 0.3 still works and still means the same thing. Two defaults moved, and there are new ones.

Changed defaults:

- `DIAL9_PROCESS_RESOURCE_USAGE_ENABLED` defaults to `true` on Unix **only with the `process-resource` feature on**. Without the feature the source does not exist.
- `DIAL9_TRACE_DIR`: each process writes into its own `{boot_id}/` subdirectory, where `boot_id` is `{4-alpha}-{pid}`.

New:

- `DIAL9_MEMORY_PROFILE_ENABLED` (`false`), `DIAL9_MEMORY_SAMPLE_RATE_BYTES` (`524288`), `DIAL9_MEMORY_TRACK_LIVESET` (`false`)
- `DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED` (`false`), `DIAL9_SOCKET_ACCEPT_QUEUES_SAMPLE_INTERVAL_MS` (`400`)
