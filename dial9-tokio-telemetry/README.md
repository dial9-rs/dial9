# dial9

[![Crates.io](https://img.shields.io/crates/v/dial9-tokio-telemetry.svg)](https://crates.io/crates/dial9-tokio-telemetry)
[![Documentation](https://docs.rs/dial9-tokio-telemetry/badge.svg)](https://docs.rs/dial9-tokio-telemetry)
![License](https://img.shields.io/crates/l/dial9-tokio-telemetry.svg)

dial9 is a microscope for Tokio (and Rust applications in general). It allows you to record a large number of events cheaply and analyze them later. By incorporating data from Tokio, the operating system, and your application, hard-to-debug problems can become obvious. "What is Tokio actually doing?" becomes readily apparent.

[Demo (Youtube)](https://www.youtube.com/watch?v=kr0RYMu57kU) | [Demo Application](https://dial9-tokio-telemetry.netlify.app/?trace=demo-trace.bin) 

<img width="1288" height="659" alt="Screenshot 2026-03-01 at 3 52 59 PM" src="https://github.com/user-attachments/assets/77225801-70b1-4aef-b064-32bc2326b1ef" href="https://dial9-tokio-telemetry.netlify.app/?trace=demo-trace.bin" />

## Quick Start

dial9 allows you to efficiently collect data from [different sources](#data-sources) then [export them out of the application](#getting-data-out-of-dial9). You can enable as many different data sources as you need to debug (or as few as you can tolerate the overhead of in production.) Most applications will want Tokio events, CPU profiling information, and a handful of application events.

Once you have data, you will want to analyze it. There are two complementary paths:
1. The `dial9` crate which provides an HTML static site which can view the trace files. The viewer is also hosted [here](https://dial9-tokio-telemetry.netlify.app/).
2. Via the agent toolkit: `dial9` ships skill documentation and scripts to allow agents to perform scripted analysis of dial9 traces.

For more information see [Analyzing Trace Files](#analyzing-trace-files)

If you are integrating dial9 into a production service, see the [`production_use` example](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/dial9-tokio-telemetry/examples/production_use.rs).

You can also find a full [example service](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/examples).

Tokio relies on `tokio_unstable` for Tokio runtime hooks and frame pointers for efficient profiling.

```toml
# .cargo/config.toml
[build]
rustflags = [
  "--cfg", "tokio_unstable",
  # For profiling, you also need:
  "-C", "force-frame-pointers=yes"
]
```

```rust,no_run
use dial9_tokio_telemetry::{main, Dial9Config, telemetry::Dial9TokioHandle};

fn my_config() -> Dial9Config {
    Dial9Config::builder()
        .on_disk_buffer("/tmp/my_traces/trace.bin")
        .max_total_size(5 * 1024 * 1024)   // keep at most 5 MiB on disk
        .max_file_size(1024 * 1024)     // optional: defaults to min(100 MiB, max_total_size / 4)
        .rotation_period(std::time::Duration::from_secs(300)) // optional: rotate every 5 min (default: 60 s)
        .with_runtime(|r| r.with_runtime_name("main").with_task_tracking(true))  // TracedRuntime knobs
        .with_tokio(|t| { t.worker_threads(4); }) // tokio knobs
        .build_or_disabled() // or use build() to handle config failures explicitly
}

#[dial9_tokio_telemetry::main(config = my_config)] // inline config function is also supported
async fn main() {
    let handle = Dial9TokioHandle::current();
    handle
        .spawn(async { /* wake events tracked */ })
        .await
        .unwrap();
}
```

For zero-code configuration in production, use `Dial9Config::from_env()`:

```rust,no_run
use dial9_tokio_telemetry::{main, Dial9Config, telemetry::Dial9TokioHandle};

fn my_config() -> Dial9Config {
    Dial9Config::from_env()
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    let handle = Dial9TokioHandle::current();
    handle.spawn(async { /* wake events tracked when enabled */ }).await.unwrap();
}
```

`from_env()` supports these local trace writer knobs:

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_ENABLED` | `false` | Master switch for installing telemetry. |
| `DIAL9_TRACE_DIR` | `/tmp/dial9-traces` | Directory for rotated trace segments. |
| `DIAL9_ROTATION_SECS` | `60` | Rotation period in seconds, measured monotonically from writer start. |
| `DIAL9_MAX_DISK_USAGE_MB` | `1024` | Total on-disk trace budget in MiB. |
| `DIAL9_MAX_FILE_SIZE_MB` | `min(100, total / 4)` | Per-file trace segment size in MiB. |

Runtime knobs:

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_TASK_TRACKING_ENABLED` | `true` | Track tasks spawned through dial9 handles. |
| `DIAL9_TOKIO_INSTRUMENTATION_ENABLED` | `true` | Install dial9's Tokio runtime hook instrumentation. |
| `DIAL9_RUNTIME_NAME` | unset | Human-readable runtime name in trace metadata. |

S3 upload knobs (`worker-s3` feature required):

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_S3_BUCKET` | unset | Upload sealed trace segments to this bucket. |
| `DIAL9_SERVICE_NAME` | binary name | Service name used in S3 keys and metadata. |
| `DIAL9_S3_PREFIX` | `dial9-traces` | S3 object key prefix. |

CPU profiling knobs (`cpu-profiling` feature required):

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_CPU_PROFILE_ENABLED` | `true` on Linux with `cpu-profiling`, `false` otherwise | Enable CPU stack sampling. |
| `DIAL9_CPU_SAMPLE_HZ` | `99` | CPU sampling frequency in Hz. |
| `DIAL9_SCHEDULE_PROFILE_ENABLED` | `true` on Linux with `cpu-profiling`, `false` otherwise | Enable per-worker scheduler event capture. Requires the [CPU profiling setup](#cpu-profiling-linux-only). |

Process resource usage knobs:

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_PROCESS_RESOURCE_USAGE_ENABLED` | `true` on Unix, `false` otherwise | Enable process resource usage sampling from `getrusage(RUSAGE_SELF)`. |
| `DIAL9_PROCESS_RESOURCE_USAGE_SAMPLE_INTERVAL_MS` | `100` | Sampling interval in milliseconds. |

Task dump knobs (capture requires the `taskdump` feature):

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_TASK_DUMP_ENABLED` | `false` | Capture async task dumps at idle yield points. |
| `DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS` | `10` | Mean idle duration for task dump sampling. |

Missing variables use defaults. Blank, invalid, or non-Unicode values emit a warning and are treated as missing. Some numeric defaults come from the underlying config builders and are listed here as the current `from_env()` behavior.

## Why dial9-tokio-telemetry?

It can be hard to understand application performance and behavior in async code. dial9 tracks Tokio, operating system and application events to create a detailed, nanosecond-by-nanosecond trace of your application behavior that you can analyze. On Linux, you can capture CPU profiles and kernel scheduling events, so you can see not just _that_ a task was delayed but _what code_ was running on the worker instead.

Compared to [tokio-console](https://github.com/tokio-rs/console), which is designed for live debugging, dial9 is designed for post-hoc analysis and to be a tool you can run in production. dial9 pushes out trace files to disk, S3 and anywhere else you configure. After a problem happens, you can come back to the trace to figure out the problem.

Compared to [tokio-metrics](https://github.com/tokio-rs/tokio-metrics), which exports aggregate counters (mean poll time, queue depth, etc.) for dashboarding and alerting, dial9 records every individual event. tokio-metrics can tell you something is wrong. dial9 can tell you _what_ is wrong. Use tokio-metrics for operational dashboards, and dial9 for debugging the root cause.

## Data sources

dial9 is fundamentally a central buffer that can collect data from different sources. You can pull in as many or as few as you want.

- [Tokio Events](#tokio-events): dial9 can capture poll, wake, and worker events from Tokio
- [Process resource usage](#process-resource-usage-unix): dial9 can sample process-level resource usage on Unix
- [CPU profiling](#cpu-profiling-linux-only): dial9 can capture linux performance counters and events to produce flamegraphs
- [Memory profiling](#memory-profiling): dial9 can sample heap allocations to produce allocation flamegraphs and detect leaks
- [Tracing spans](#tracing-span-events-opt-in): dial9 can capture tracing spans to bring tracing context into your trace files
- [Task dumps](#task-dumps-linux-only): dial9 can capture a task dump (a backtrace when your future goes idle) to determine what it is waiting for when idle
- [Custom events](#custom-events): dial9 can record custom application events into the trace


### Tokio events
`dial9` uses Tokio runtime hooks to record events on each `poll`, task `spawn` and when runtime workers park and unpark. If you use `dial9`'s [`spawn`](https://docs.rs/dial9-tokio-telemetry/latest/dial9_tokio_telemetry/telemetry/fn.spawn.html) your future will be instrumented to capture two additional pieces of info:
1. The wake event, when your future was _ready_ to run vs. when Tokio actually started running it.
2. A "task dump", a stack trace of what your future was doing when it went idle.

`dial9` can instrument a single runtime by using `TracedRuntime` or by using the `dial9_tokio_telemetry::main` macro.

```rust
# #[cfg(feature = "worker-s3")]
# mod inner {
use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::background_task::s3::S3Config;

fn my_config() -> Dial9Config {
    let s3_config = S3Config::builder()
        .bucket("my-trace-bucket")
        .service_name("my-service")
        .build();

    Dial9Config::builder()
        .on_disk_buffer("/tmp/my_traces/trace.bin")
        .max_file_size(100 * 1024 * 1024)
        .max_total_size(500 * 1024 * 1024)
        .with_tokio(|t| { t.worker_threads(4); })
        .with_runtime(|r| {
            r.with_task_tracking(true)
             .with_s3_uploader(s3_config)
        })
        .build_or_disabled()
}
# }
```

#### Instrumenting multiple runtimes

`dial9` can also capture data from multiple runtimes. 
See [`examples/thread_per_core.rs`](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/dial9-tokio-telemetry/examples/thread_per_core.rs) and [`examples/multi_runtime.rs`](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/dial9-tokio-telemetry/examples/multi_runtime.rs) for complete examples.

### Process resource usage (Unix)

Programmatic builders leave process resource usage sampling disabled unless you
opt in:

```rust,ignore
use dial9_tokio_telemetry::telemetry::{ProcessResourceUsageConfig, TracedRuntime};

let (runtime, guard) = TracedRuntime::builder()
    .with_process_resource_usage(ProcessResourceUsageConfig::default())
    .build_and_start(tokio::runtime::Builder::new_multi_thread(), writer)?;
```

Or use `TelemetryCore::builder().process_resource_usage(...)` directly.

`Dial9Config::from_env()` enables this by default on Unix when telemetry itself
is enabled. To opt out, set:

```text
DIAL9_PROCESS_RESOURCE_USAGE_ENABLED=false
```

### CPU profiling (Linux only)

dial9 supports two forms of CPU profiling:
- "traditional" CPU profiling / flamegraphs: dial9 can use Linux perf events with a fallback to `ctimer` for containerized environments. This allows you to get application stacks with attached metadata. You can see exactly what was happening during a long poll or see a flamegraph for one specific Tokio task.
- schedule profiling: With `perf_event_paranoid <= 1` dial9 can capture stack traces when your code is moved off-CPU by the kernel. This is extremely helpful when diagnosing issues in async applications: If your future is moved off CPU while polling this is almost always an indication of a problem.

Both of these events are tied to the precise instant and thread that they happened on, so you can compare what was different between degraded and normal performance.

#### Application Requirements

**Enable the `cpu-profiling` feature**:
```toml
[dependencies]
dial9-tokio-telemetry = { version = "0.3", features = ["cpu-profiling"] }
```

**Enable frame pointers**:
```toml
# .cargo/config.toml
[build]
rustflags = ["--cfg", "tokio_unstable", "-C", "force-frame-pointers=yes"]
```

**Set `with_cpu_profiling`**:

```rust,ignore
use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::telemetry::cpu_profile::{CpuProfilingConfig, SchedEventConfig};
Dial9Config::builder()
    // ...
    .with_runtime(|r| {
        // Enable normal CPU profiles
        .with_cpu_profiling(CpuProfilingConfig::default())
        // Enable scheduling profiling
        .with_sched_events(SchedEventConfig::default().include_kernel(true))
    })
    // ...
```

To use dial9 as a CPU profiler without installing Tokio runtime hooks, keep
telemetry enabled and disable only Tokio instrumentation:

```rust,ignore
use dial9_tokio_telemetry::telemetry::cpu_profile::CpuProfilingConfig;
use dial9_tokio_telemetry::telemetry::TracedRuntime;

let (runtime, guard) = TracedRuntime::builder()
    .with_cpu_profiling(CpuProfilingConfig::default())
    .with_tokio_instrumentation(false)
    .build_and_start(tokio::runtime::Builder::new_multi_thread(), writer)?;
```

You can also use `TelemetryCore::builder()` directly when you only need the
telemetry session and want to decide separately whether to build or attach any
Tokio runtime.

Equivalent env config:

```text
DIAL9_ENABLED=true
DIAL9_CPU_PROFILE_ENABLED=true
DIAL9_TOKIO_INSTRUMENTATION_ENABLED=false
```

In this mode, dial9 does not install Tokio runtime hooks. APIs that depend on
those hooks will not observe runtime context.

#### System requirements
- `perf_event_paranoid`: CPU profiling requires <= 2. `sched_events` requires <= 1.
    ```bash
    # check current value
    cat /proc/sys/kernel/perf_event_paranoid
    
    # allow CPU sampling and scheduler event tracking
    sudo sysctl kernel.perf_event_paranoid=1
    ```

- Kernel stack traces: To enable dial9 to symbolize traces that go into kernel functions `kernel.kptr_restrict` must be 0 for non-root, or else they will show up like: `[kernel] 0xffffffff81336901`:
  ```bash
  sudo sysctl kernel.kptr_restrict=0
  ```

### Memory profiling

dial9 can sample heap allocations using [probabilistic sampling](docs/design/memory-profiling.md) and capture stack traces for each sample. This produces allocation flamegraphs showing where memory is being allocated. With liveset tracking enabled, you can also detect memory leaks by seeing which allocations are never freed. The agent toolkit includes skills for automated memory profiling analysis.

**Enable the `memory-profiling` feature:**
```toml
[dependencies]
dial9-tokio-telemetry = { version = "0.3", features = ["memory-profiling"] }
```

**Install the allocator and profiler:**

```rust,no_run
use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};
use dial9_tokio_telemetry::telemetry::Dial9Handle;

// Install as the global allocator. Zero-cost passthrough until
// MemoryProfiler::install() is called.
#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

// If you already use jemalloc or mimalloc, wrap it instead:
// static ALLOC: Dial9Allocator<tikv_jemallocator::Jemalloc> =
//     Dial9Allocator::new(tikv_jemallocator::Jemalloc);

# fn example(handle: Dial9Handle) {
let config = MemoryProfilingConfig::builder()
    .sample_rate_bytes(512 * 1024)  // sample ~every 512 KiB allocated (default)
    .track_liveset(true)            // track frees for leak detection
    .build();

let _guard = MemoryProfiler::from_config(config)
    .install(handle)
    .expect("failed to install memory profiler");
# }
# fn main() {}
```

The `sample_rate_bytes` controls how frequently allocations are sampled. At the default of 512 KiB, a service allocating 1 GB/s produces ~2000 samples/sec. Set to `1` to sample every allocation (useful for tests, not production).

#### Liveset tracking and leak detection

When `track_liveset(true)` is set, dial9 records every deallocation so it can determine which sampled allocations are still live at any point in the trace. This is how you find memory leaks: allocations that appear in the liveset and grow over time without being freed.

> **Caveat:** At very high deallocation rates the free queue can overflow. When a free event is dropped, the corresponding allocation remains in the liveset even if it was actually freed. The viewer and agent skills will flag when overflow is detected in the trace; if you see suspicious liveset growth in a high-throughput service, check for overflow warnings before concluding you have a real leak.

#### Performance

| Path | Overhead per call |
| --- | --- |
| Unsampled allocation (~99.9%) | ~5 ns |
| Sampled allocation (~0.1%) | ~1 µs (stack capture) |
| Every deallocation (liveset on) | ~200 ns |
| Before `install()` | ~1 ns (null check) |

Without liveset tracking, the profiler adds negligible overhead. With liveset tracking, the ~200 ns per free is the dominant cost — budget accordingly for allocation-heavy services.

> **Note:** Memory profiling is not yet configurable via `Dial9Config::from_env()`. Use the programmatic API shown above. See [#457](https://github.com/dial9-rs/dial9-tokio-telemetry/issues/457) for tracking.

### Tracing span events (opt-in)

**Enable the `tracing-layer` feature:**
```toml
[dependencies]
dial9-tokio-telemetry = { version = "0.3", features = ["tracing-layer"] }
```

**Use tracing_subscriber to connect the `Dial9TokioLayer`:**
```rust
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use tracing_subscriber::prelude::*;

tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(
        Dial9TokioLayer::new().with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target("my_app", tracing::Level::TRACE)
                .with_default(tracing::Level::ERROR),
        ),
    )
    .init();
```

Careful filtering of the data you send to dial9 strongly recommended. dial9 doesn't need _all_ the data, only enough to correlate with other data sources. Libraries like the AWS SDK emit many internal spans that can produce over 100K events per second. The example above captures only spans from my_app. Each span enter+exit costs ~300ns total (~50-100ns is dial9 encoding overhead).


### Task dumps (Linux only)

`dial9` can capture async backtraces at yield points. This is the Tokio equivalent of scheduling events: You can see the stack trace your future was at when it went idle.

> Note: The taskdump feature requires Tokio's upstream taskdump support, which only compiles on Linux (aarch64, x86, x86_64). Enabling it on other targets is a hard compile error from Tokio.

```rust
# #[cfg(feature = "taskdump")]
# mod inner {
# use std::time::Duration;
use dial9_tokio_telemetry::{Dial9Config, telemetry::TaskDumpConfig};

fn my_config() -> Dial9Config {
    Dial9Config::builder()
        // ...
        .with_runtime(|r| {
            r.with_task_tracking(true)
             .with_task_dumps(TaskDumpConfig::builder().idle_threshold(Duration::from_millis(10)).build())
        })
        .build_or_disabled()
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() { /* ... */ }
# }
# fn main() {}
```

> Performance note: Task dumps currently produce one extra wake per capture and are more likely than other features to degrade performance. Measure overhead in your environment before enabling in latency-sensitive paths.

### Custom events

You can emit your own application-level events into the trace alongside the built-in runtime events. Define a struct with `#[derive(TraceEvent)]` and call `record_event`:

```rust,no_run
# fn main() {
use dial9_trace_format::TraceEvent;
use dial9_tokio_telemetry::telemetry::{clock_monotonic_ns, Dial9Handle};

#[derive(TraceEvent)]
struct RequestCompleted {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    status_code: u32,
    latency_us: u64,
    /// Optional fields use 1 byte on the wire when absent.
    error_message: Option<String>,
}

# let handle: Dial9Handle = todo!();
handle.record_event(RequestCompleted {
    timestamp_ns: clock_monotonic_ns(),
    status_code: 200,
    latency_us: 1500,
    error_message: None,
});
# }
```

### Custom event callbacks

You can also register a callback that runs from dial9's flush thread and emits
custom events. This is useful for draining application-owned queues or taking
periodic snapshots without passing a [`Dial9Handle`] through your code:

```rust,ignore
use dial9_trace_format::TraceEvent;
use dial9_tokio_telemetry::telemetry::{CustomEventsConfig, TracedRuntime};

#[derive(TraceEvent)]
struct CacheEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    entries: u64,
}

let (_runtime, _guard) = TracedRuntime::builder()
    .with_custom_events(CustomEventsConfig::default(), move |ctx| {
        while let Ok(event) = rx.try_recv() {
            ctx.record_event(event);
        }
    })
    .build_and_start(builder, writer)?;
```

`CustomEventsConfig::default()` runs the callback every flush cycle
while telemetry is enabled, which fits drain-style callbacks. For polling-style
callbacks, configure `minimum_interval(...)` to limit how often dial9 invokes
the callback.

### Custom Runtime Hooks

dial9 installs callbacks on all 8 Tokio runtime hooks to collect telemetry. If you need to run your own logic alongside dial9's instrumentation, use `with_tokio_hooks`:

```rust,no_run
use dial9_tokio_telemetry::telemetry::{InMemoryWriter, TracedRuntime};

let mut builder = tokio::runtime::Builder::new_multi_thread();
builder.worker_threads(4).enable_all();

let (runtime, guard) = TracedRuntime::builder()
    .with_tokio_hooks(|hooks| {
        hooks.on_thread_start(|| {
            println!("Worker thread started");
        });
        hooks.on_thread_stop(|| {
            println!("Worker thread stopping");
        });
        // Also available: on_thread_park, on_thread_unpark,
        // on_task_spawn, on_task_terminate, on_before_task_poll, on_after_task_poll
    })
    .build_and_start(builder, InMemoryWriter::new(16 * 1024 * 1024).unwrap())
    .unwrap();
```

dial9's internal hooks always run first, then your callbacks fire in registration order. This ensures `Dial9Handle::current()` is available in your `on_thread_start` callback. Registering the same hook multiple times stacks the callbacks — all of them will fire.

**Important:** Do not set hooks directly via `tokio::runtime::Builder::on_thread_start()` etc. — dial9 will overwrite them. Always use `with_tokio_hooks` to compose your callbacks with dial9's instrumentation.

## Getting data out of dial9

dial9 is recording data to in memory buffers and eventually to disk. For most applications, they would like the data to go somewhere else. `dial9` has a built in exporter for S3 and it is also possible to write your own exporter.

### Exporting data to S3
dial9 has a built-in S3 exporter. When segments are sealed, symbolized, and compressed they will be uploaded to S3 by a background thread. The `dial9` viewer includes a browser to browse the traces stored on S3.

**Enable the `worker-s3` feature:**
```toml
[dependencies]
dial9-tokio-telemetry = { version = "0.3", features = ["worker-s3"] }
```

**Create the S3 bucket**: Ensure your application has `s3:PutObject` and `s3:ListBucket` permissions to the bucket.

**Set `with_s3_uploader`:**
```rust,no_run
# #[cfg(feature = "worker-s3")]
# mod inner {
use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::background_task::s3::S3Config;

fn my_config() -> Dial9Config {
    let s3_config = S3Config::builder()
        .bucket("my-trace-bucket")
        .service_name("my-service")
        .build();

    Dial9Config::builder()
        .on_disk_buffer("/tmp/dial9/trace.bin")
        .max_total_size(1 << 30)
        .with_runtime(|r| {
            r.with_task_tracking(true)
             .with_s3_uploader(s3_config)
        })
        .build_or_disabled()
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    // your async code here
}
// on shutdown: flushes, seals final segment, worker drains remaining to S3
# }
# fn main() {}
```

When you use `#[dial9_tokio_telemetry::main]`, this shutdown drain happens
automatically once `main` returns: the macro drops the runtime and then calls
`graceful_shutdown` with a 1s deadline so the final segment is uploaded. Tune it
with `.graceful_shutdown(Duration)` on the config builder, or turn it off with
`.disable_graceful_shutdown()`. If you build a `TracedRuntime` by hand instead of
using the macro, call `guard.graceful_shutdown(timeout)` yourself after the
runtime is dropped.

### Running without disk (in-memory)

To run with **no filesystem dependency** (disk unavailable, read-only, or unwelcome) use `InMemoryWriter`. Encoded segments stay in process memory and are shipped by the same processor pipeline (S3, custom, ...).

```rust,no_run
# #[cfg(feature = "worker-s3")]
# mod inner {
use dial9_tokio_telemetry::background_task::s3::S3Config;
use dial9_tokio_telemetry::telemetry::{InMemoryWriter, TracedRuntime};

# fn example() -> std::io::Result<()> {
let writer = InMemoryWriter::new(16 * 1024 * 1024)?; // 16 MiB RAM budget

let mut tk = tokio::runtime::Builder::new_multi_thread();
tk.enable_all();

let s3 = S3Config::builder().bucket("my-bucket").service_name("svc").build();
let (runtime, guard) = TracedRuntime::builder()
    .with_custom_pipeline(|p| p.gzip().s3(s3))
    .build_and_start(tk, writer)?;
# let _ = (runtime, guard);
# Ok(())
# }
# }
# fn main() {}
```

`max_total_size` bounds the in-memory buffers: if a slow exporter falls behind, the oldest sealed segments are dropped rather than blocking recording. See [`examples/in_memory_pipeline.rs`](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/dial9-tokio-telemetry/examples/in_memory_pipeline.rs).

### Exporting data to other destinations

For custom upload destinations or post-processing (e.g. shipping to a different object store, running analysis on each segment), you can replace the built-in pipeline entirely with `with_custom_pipeline`. See [`examples/custom_pipeline.rs`](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/dial9-tokio-telemetry/examples/custom_pipeline.rs) for a complete example.

## Analyzing trace files
[`dial9`](https://github.com/dial9-rs/dial9-tokio-telemetry/tree/main/dial9-viewer) is a CLI for browsing and analyzing traces. Use `dial9 serve` to start a local web UI that visualizes traces from a directory or S3 bucket. [Here's a demo.](https://www.youtube.com/watch?v=kr0RYMu57kU)

```bash
# Install
cargo install --locked dial9
# or, for pre-built binaries:
cargo binstall dial9

# Serve traces from a local directory
dial9 serve --local-dir /tmp/my_traces

# Serve traces from S3
dial9 serve --bucket my-trace-bucket
```

### Agent toolkit

`dial9` also ships skill documentation and JS analysis modules for scripted trace analysis.

```bash
# Print the agent skill overview
dial9 agents

# Unpack all skills to a directory
dial9 agents skills /path/to/skills

# Extract the JS analysis toolkit
dial9 agents toolkit /path/to/toolkit
node /path/to/toolkit/analyze.js /tmp/my_traces/
```

If you use [Symposium](https://symposium.dev), skills auto-install when your project depends on `dial9-tokio-telemetry`:

```bash
cargo agents sync
```

## License

This project is licensed under the Apache-2.0 License.
