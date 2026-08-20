# dial9

[![Crates.io](https://img.shields.io/crates/v/dial9.svg)](https://crates.io/crates/dial9)
[![Documentation](https://docs.rs/dial9/badge.svg)](https://docs.rs/dial9)
![License](https://img.shields.io/crates/l/dial9.svg)

dial9 is a microscope for Tokio and Rust applications in general. It allows you to record a large number of events cheaply and analyze them later. By incorporating data from Tokio, the operating system, and your application, hard-to-debug problems can become obvious. "What is Tokio actually doing?" becomes readily apparent.

[Demo (Youtube)](https://www.youtube.com/watch?v=kr0RYMu57kU) | [Demo Application](https://dial9-tokio-telemetry.netlify.app/?trace=demo-trace.bin) 

<img width="1288" height="659" alt="Screenshot 2026-03-01 at 3 52 59 PM" src="https://github.com/user-attachments/assets/77225801-70b1-4aef-b064-32bc2326b1ef" href="https://dial9-tokio-telemetry.netlify.app/?trace=demo-trace.bin" />

## Quick Start

dial9 allows you to efficiently collect data from [different sources](#data-sources) then [export them out of the application](#getting-data-out-of-dial9). You can enable as many different data sources as you need to debug (or as few as you can tolerate the overhead of in production.) Most applications will want Tokio events, CPU profiling information, and a handful of application events.

Once you have data, you will want to analyze it. There are two complementary paths:
1. The `dial9` crate which provides an HTML static site which can view the trace files. The viewer is also hosted [here](https://dial9-tokio-telemetry.netlify.app/).
2. Via the agent toolkit: `dial9` ships skill documentation and scripts to allow agents to perform scripted analysis of dial9 traces.

For more information see [Analyzing Trace Files](#analyzing-trace-files)

If you are integrating dial9 into a production service, see the [`production_use` example](https://github.com/dial9-rs/dial9/blob/HEAD/dial9/examples/production_use.rs).

You can also find a full [example service](https://github.com/dial9-rs/dial9/blob/HEAD/examples).

dial9's Tokio instrumentation is built on runtime hooks that Tokio only exposes
under `tokio_unstable`. With the flag set, dial9 sees every task on the runtime:
poll spans, task spawn and terminate, and per-worker queue depth. Frame pointers are required by the `cpu-profiling` and `memory-profiling` features, which capture stacks with a frame-pointer unwinder. These flags go in your [Cargo build configuration](https://doc.rust-lang.org/cargo/reference/config.html) (for example `.cargo/config.toml` or the `RUSTFLAGS` environment variable), not in `Cargo.toml`.

```toml
# .cargo/config.toml
[build]
rustflags = [
  "--cfg", "tokio_unstable",
  # Required by the cpu-profiling and memory-profiling features:
  "-C", "force-frame-pointers=yes"
]
```

The Tokio instrumentation still works without the flag, with narrower task coverage.
CPU profiling, worker timelines, wake causality, memory and application events are unaffected. What narrows is task visibility: poll events come from dial9's own spawn helpers rather than the runtime, so they cover tasks started with `dial9::spawn`, `spawn_in`, `block_on` or `spawn_with` and miss the rest. Task spawn and terminate events and per-worker queue depth are not accessible without the flag.

```rust,no_run
use std::io;
use dial9::{AttachedRuntime, Dial9HandleTokioExt, Dial9TokioHandle, DiskBuffer, TokioAttachOptions};

fn my_config() -> io::Result<AttachedRuntime> {
    let writer = DiskBuffer::builder()
        .base_path("/tmp/my_traces")
        .max_total_size(5 * 1024 * 1024)   // keep at most 5 MiB on disk
        .max_file_size(1024 * 1024)     // optional: defaults to min(100 MiB, max_total_size / 4)
        .rotation_period(std::time::Duration::from_secs(300)) // optional: rotate every 5 min (default: 60 s)
        .build();
    // Downgrades to a disabled recorder if the writer can't be created; use
    // `dial9::recorder(writer?)` instead to surface writer errors explicitly.
    let recorder = dial9::recorder_or_disabled(writer)
        .segment_metadata([("service".to_string(), "checkout".to_string())])
        .segment_metadata([(
            "application.version".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        )])
        .build();

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

#[dial9::main(config = my_config)] // inline config function is also supported
async fn main() {
    let handle = Dial9TokioHandle::current();
    handle
        .spawn(async { /* wake events tracked */ })
        .await
        .unwrap();
}
```

Use [`RecorderBuilder::segment_metadata`](https://docs.rs/dial9/latest/dial9/struct.RecorderBuilder.html#method.segment_metadata)
for static context that should be available when any rotated segment is loaded
independently, such as the service, host, deployment, or compiled application
version. Calls are merged, including calls made by integration layers; when a
key is repeated, the later value wins.

For zero-code configuration in production, use `dial9::recorder_from_env`:

```rust,no_run
use dial9::Dial9TokioHandle;

#[dial9::main(config = dial9::recorder_from_env)]
async fn main() {
    let handle = Dial9TokioHandle::current();
    handle.spawn(async { /* wake events tracked when enabled */ }).await.unwrap();
}
```

Use `dial9::recorder_from_env_with` to keep the env-driven recording but configure the
runtime it builds:

```rust,no_run
#[dial9::main(config = || dial9::recorder_from_env_with(|t| { t.worker_threads(8); }))]
async fn main() {
    /* ... */
}
```

Every `config` returns `std::io::Result<dial9::AttachedRuntime>`: the recorder plus a runtime
attached to it. Attach through the recorder's handle, configuring the Tokio builder yourself:

```rust,no_run
use dial9::{Dial9HandleTokioExt, TokioAttachOptions};
# fn writer() -> std::io::Result<dial9::DiskBuffer> { unimplemented!() }
#[dial9::main(config = || {
    let recorder = dial9::recorder_or_disabled(writer()).build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    let runtime = recorder
        .handle()
        .attach_tokio_runtime(builder, TokioAttachOptions::default())?;

    Ok((recorder, runtime))
})]
async fn main() {
    /* ... */
}
```

`recorder_from_env` supports these local trace writer knobs:

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_ENABLED` | `false` | Master switch for installing telemetry. |
| `DIAL9_TRACE_DIR` | `/tmp/dial9-traces` | Directory for rotated trace segments. Each process writes into its own `{boot_id}/` subdirectory, where `boot_id` is `{4-alpha}-{pid}`. |
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

Memory profiling knobs (`memory-profiling` feature required; your binary must still install `Dial9Allocator` as its `#[global_allocator]`):

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_MEMORY_PROFILE_ENABLED` | `false` | Enable memory allocation sampling. |
| `DIAL9_MEMORY_SAMPLE_RATE_BYTES` | `524288` | Mean bytes between sampled allocations. |
| `DIAL9_MEMORY_TRACK_LIVESET` | `false` | Track frees for leak detection. |

Process resource usage knobs (`process-resource` feature required):

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_PROCESS_RESOURCE_USAGE_ENABLED` | `true` on Unix with `process-resource`, `false` otherwise | Enable process resource usage sampling from `getrusage(RUSAGE_SELF)`. |
| `DIAL9_PROCESS_RESOURCE_USAGE_SAMPLE_INTERVAL_MS` | `100` | Sampling interval in milliseconds. |

Socket accept queue knobs (`linux-socket` feature required, Linux only):

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED` | `false` | Enable TCP accept queue snapshots from Linux sock_diag. |
| `DIAL9_SOCKET_ACCEPT_QUEUES_SAMPLE_INTERVAL_MS` | `400` | Sampling interval in milliseconds. |

Task dump knobs (capture requires the `taskdump` feature):

| Name | Default | Meaning |
| --- | --- | --- |
| `DIAL9_TASK_DUMP_ENABLED` | `false` | Capture async task dumps at idle yield points. |
| `DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS` | `10` | Mean idle duration for task dump sampling. |

Missing variables use defaults. Blank, invalid, or non-Unicode values emit a warning and are treated as missing. Some numeric defaults come from the underlying config builders and are listed here as the current `recorder_from_env` behavior.

## Why dial9?

It can be hard to understand application performance and behavior in async code. dial9 tracks Tokio, operating system and application events to create a detailed, nanosecond-by-nanosecond trace of your application behavior that you can analyze. On Linux, you can capture CPU profiles and kernel scheduling events, so you can see not just _that_ a task was delayed but _what code_ was running on the worker instead.

Compared to [tokio-console](https://github.com/tokio-rs/console), which is designed for live debugging, dial9 is designed for post-hoc analysis and to be a tool you can run in production. dial9 pushes out trace files to disk, S3 and anywhere else you configure. After a problem happens, you can come back to the trace to figure out the problem.

Compared to [tokio-metrics](https://github.com/tokio-rs/tokio-metrics), which exports aggregate counters (mean poll time, queue depth, etc.) for dashboarding and alerting, dial9 records every individual event. tokio-metrics can tell you something is wrong. dial9 can tell you _what_ is wrong. Use tokio-metrics for operational dashboards, and dial9 for debugging the root cause.

## Data sources

dial9 is fundamentally a central buffer that can collect data from different sources. You can pull in as many or as few as you want.

- [Tokio Events](#tokio-events): dial9 can capture poll, wake, and worker events from Tokio
- [Process resource usage](#process-resource-usage-unix): dial9 can sample process-level resource usage on Unix
- [Socket accept queues](#socket-accept-queues-linux-only): dial9 can sample pending TCP listener connections and backlog limits on Linux
- [CPU profiling](#cpu-profiling-linux-only): dial9 can capture linux performance counters and events to produce flamegraphs
- [Memory profiling](#memory-profiling): dial9 can sample heap allocations to produce allocation flamegraphs and detect leaks
- [Metrique metrics](#metrique-metrics-opt-in): dial9 can record metrique unit-of-work metric entries alongside your EMF/JSON pipeline
- [Task dumps](#task-dumps-linux-only): dial9 can capture a task dump (a backtrace when your future goes idle) to determine what it is waiting for when idle
- [Custom events](#custom-events): dial9 can record custom application events into the trace


### Tokio events
`dial9` uses Tokio runtime hooks to record events on each `poll`, task `spawn` and when runtime workers park and unpark. If you use `dial9`'s [`spawn`](https://docs.rs/dial9/latest/dial9/fn.spawn.html) your future will be instrumented to capture two additional pieces of info:
1. The wake event, when your future was _ready_ to run vs. when Tokio actually started running it.
2. A "task dump", a stack trace of what your future was doing when it went idle.

`recorder.handle().attach_tokio_runtime(..)` takes a Tokio runtime builder you configured, installs
dial9's hooks on it, and builds it. Pair the recorder with the runtime to get a
`dial9::AttachedRuntime`, which is what a `#[dial9::main]` config must produce.

Driving that runtime yourself, reach for [`dial9::block_on`](https://docs.rs/dial9/latest/dial9/fn.block_on.html) rather than
`Runtime::block_on`. Poll and wake events come from Tokio's per-task hooks, and `Runtime::block_on` would
polls its future outside any task, so that future and everything awaited inline under it would be absent
from the trace. `dial9::block_on` spawns it first. `#[dial9::main]` already does this for you.

```rust,no_run
# #[cfg(feature = "worker-s3")]
# mod inner {
use std::io;

use dial9::s3::S3Config;
use dial9::{AttachedRuntime, Dial9HandleTokioExt, DiskBuffer, RecorderPipelineExt, TokioAttachOptions};

fn my_config() -> io::Result<AttachedRuntime> {
    let s3_config = S3Config::builder()
        .bucket("my-trace-bucket")
        .service_name("my-service")
        .build();

    let writer = DiskBuffer::builder()
        .base_path("/tmp/my_traces")
        .max_file_size(100 * 1024 * 1024)
        .max_total_size(500 * 1024 * 1024)
        .build()
        .expect("build trace writer");
    let recorder = dial9::recorder(writer)
        .with_s3_uploader(s3_config)
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all().worker_threads(4);
    let runtime = recorder.handle().attach_tokio_runtime(
        builder,
        TokioAttachOptions::builder().task_tracking_enabled(true).build(),
    )?;

    Ok((recorder, runtime))
}
# }
# fn main() {}
```

#### Instrumenting multiple runtimes

`dial9` can also capture data from multiple runtimes: the handle attaches as many as you like and
they all feed the same trace. Clone it and each thread can build its own runtime.
See [`examples/thread_per_core.rs`](https://github.com/dial9-rs/dial9/blob/HEAD/dial9/examples/thread_per_core.rs) and [`examples/multi_runtime.rs`](https://github.com/dial9-rs/dial9/blob/HEAD/dial9/examples/multi_runtime.rs) for complete examples.

### Process resource usage (Unix)

With the `process-resource` feature, dial9 can sample process-level resource
usage from `getrusage(RUSAGE_SELF)`. Programmatic builders leave it disabled
unless you opt in:

```rust,no_run
use dial9::process::ProcessResourceUsageConfig;
use dial9::RecorderPerfExt;
# let writer = dial9::MemoryBuffer::new(1 << 20).unwrap();
let recorder = dial9::recorder(writer)
    .with_process_resource_usage(ProcessResourceUsageConfig::default())
    .build();
```

`dial9::recorder_from_env` enables it by default on Unix when the
`process-resource` feature is on. To opt out, set:

```text
DIAL9_PROCESS_RESOURCE_USAGE_ENABLED=false
```

### Socket accept queues (Linux only)

With the `linux-socket` feature, dial9 can sample TCP listener accept
queues from Linux `sock_diag`. Each snapshot records the listener address,
pending connection count, and backlog limit for sockets owned by the current
process.

Programmatic builders leave socket accept queue sampling disabled unless you
opt in:

```rust,no_run
use dial9::socket::SocketAcceptQueuesConfig;
use dial9::RecorderPerfExt;
# let writer = dial9::MemoryBuffer::new(1 << 20).unwrap();
let recorder = dial9::recorder(writer)
    .with_socket_accept_queues(SocketAcceptQueuesConfig::default())
    .build();
```

`dial9::recorder_from_env` also leaves this source disabled by default. To opt
in, set:

```text
DIAL9_SOCKET_ACCEPT_QUEUES_ENABLED=true
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
dial9 = { version = "0.5", features = ["cpu-profiling"] }
```

**Enable frame pointers**:
```toml
# .cargo/config.toml
[build]
rustflags = ["--cfg", "tokio_unstable", "-C", "force-frame-pointers=yes"]
```

**Enable CPU profiling** (`.with_cpu_profiling` on the recorder):

```rust,no_run
use dial9::cpu::{CpuProfilingConfig, SchedEventConfig};
use dial9::RecorderPerfExt;
# let writer = dial9::MemoryBuffer::new(1 << 20).unwrap();
let recorder = dial9::recorder(writer)
    // Enable normal CPU profiles
    .with_cpu_profiling(CpuProfilingConfig::default())
    // Enable per-worker scheduler event capture
    .with_sched_events(SchedEventConfig::default().include_kernel(true))
    .build();
```

By default, dial9 tries the perf backend and falls back to ctimer if
`perf_event_open` is blocked. You can select the backend explicitly:

```rust,no_run
use dial9::cpu::{CpuProfilingConfig, EventSource};

// Use ctimer directly — zero thread lifecycle overhead, ideal for workloads
// with high thread churn (e.g. saturated block_in_place usage).
let ctimer = CpuProfilingConfig::with_ctimer_backend();

// Require perf — fail instead of silently degrading. Needed for kernel
// stacks or hardware event sources.
let perf = CpuProfilingConfig::with_perf_backend()
    .event_source(EventSource::SwCpuClock)
    .include_kernel(true);
```

To use dial9 as a CPU profiler without installing Tokio runtime hooks, build a
recorder and don't attach a runtime to it:

```rust,no_run
use dial9::cpu::CpuProfilingConfig;
use dial9::RecorderPerfExt;
# let writer = dial9::MemoryBuffer::new(1 << 20).unwrap();
let recorder = dial9::recorder(writer)
    .with_cpu_profiling(CpuProfilingConfig::default())
    .build();
recorder.enable();
```

If you do attach a runtime and want the CPU profiler without the runtime hooks,
set `tokio_instrumentation_enabled(false)` in `TokioAttachOptions`.

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

dial9 can sample heap allocations using [probabilistic sampling](https://github.com/dial9-rs/dial9/blob/HEAD/docs/design/memory-profiling.md) and capture stack traces for each sample. This produces allocation flamegraphs showing where memory is being allocated. With liveset tracking enabled, you can also detect memory leaks by seeing which allocations are never freed. The agent toolkit includes skills for automated memory profiling analysis.

**Enable the `memory-profiling` feature:**
```toml
[dependencies]
dial9 = { version = "0.5", features = ["memory-profiling"] }
```

**Enable frame pointers** (allocation stacks are captured with a frame-pointer unwinder):
```toml
# .cargo/config.toml
[build]
rustflags = ["--cfg", "tokio_unstable", "-C", "force-frame-pointers=yes"]
```

**Install the allocator and profiler:**

```rust,no_run
use dial9::memory::{Dial9Allocator, MemoryProfiler, MemoryProfilingConfig};
use dial9::Dial9Handle;

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

`dial9::recorder_from_env` can install the profiler when `DIAL9_MEMORY_PROFILE_ENABLED=true`, but your binary must still declare `Dial9Allocator` as shown above so allocations pass through dial9's hook.

### Metrique metrics (opt-in)

If your service publishes unit-of-work metrics with [metrique](https://docs.rs/metrique), dial9 can record every entry into the trace as a peer of your existing EMF/JSON pipeline. Each event is pinned to the thread and task that served the request, with start and end timestamps, so per-request metrics land on the same timeline as polls, wakes, and spans.

**Enable the `metrique-sink` feature** (with `tokio` also on, events carry the task id; the sink itself does not need a tokio runtime):
```toml
[dependencies]
dial9 = { version = "0.5", features = ["metrique-sink", "tokio"] }
```

**Opt an entry in and tee the stream:**
```rust,ignore
use dial9::metrique_sink::{Dial9Context, Dial9Stream};
use metrique::unit_of_work::metrics;

#[metrics(rename_all = "PascalCase")]
struct RequestMetrics {
    // Including a Dial9Context opts this entry into the trace.
    #[metrics(flatten)]
    dial9: Dial9Context,

    #[metrics(flags(dial9::Interned))]
    operation: &'static str,

    latency_ms: u64,
    success: bool,

    // Keep bulky or high-cardinality fields out of the trace.
    #[metrics(flags(dial9::Skip))]
    debug_blob: String,
}

// dial9 as a peer of the existing pipeline. `tee` also keeps dial9's own
// `dial9.` fields out of the EMF output:
let _join = ServiceMetrics::attach_to_stream(
    Dial9Stream::tee(&handle, emf_stream),
);

// Use normally.
let mut m = RequestMetrics {
    dial9: Dial9Context::capture(),
    /* ... */
};
```

Entries without a `Dial9Context` record nothing, so teeing the sink into an existing pipeline only picks up the entries you opt in.

If adding a field to the entry is awkward (a shared struct, or dial9 support you want to switch from one place), attach the context from the outside instead and leave the struct alone:

```rust,ignore
use dial9::metrique_sink::Dial9EntryExt;

// `append_on_drop_dial9` in place of `append_on_drop`; field access reaches
// through the wrapper, so the rest of the call site is unchanged.
let mut m = RequestMetrics { operation: "GetPet", latency_ms: 0 }
    .append_on_drop_dial9(ServiceMetrics::sink());
m.latency_ms = 5;
```

Field units (from `#[metrics(unit = ..)]` or the value type) are carried into the trace and shown by the viewer. Capture costs a few tens of nanoseconds on the request path; encoding happens on the metrique flush thread. Entries the sink cannot describe are not recorded (hand-written `Entry` impls without a `descriptors()` impl, and entries containing `Flex` dynamic-key fields); histogram fields are left out individually. See the `dial9::metrique_sink` module docs for measured overhead and current limitations. A runnable example is at [`examples/metrique_metrics.rs`](https://github.com/dial9-rs/dial9/blob/HEAD/dial9/examples/metrique_metrics.rs).


### Task dumps (Linux only)

`dial9` can capture async backtraces at yield points. This is the Tokio equivalent of scheduling events: You can see the stack trace your future was at when it went idle.

> Note: The taskdump feature requires Tokio's upstream taskdump support, which only compiles on Linux (aarch64, x86, x86_64) and only under `--cfg tokio_unstable`. Enabling it on another target, or without the flag, is a hard compile error from Tokio.

```rust,no_run
use std::io;
use std::time::Duration;
use dial9::{AttachedRuntime, Dial9HandleTokioExt, DiskBuffer};
use dial9::{TaskDumpConfig, TokioAttachOptions};

fn my_config() -> io::Result<AttachedRuntime> {
    let writer = DiskBuffer::builder()
        .base_path("/tmp/dial9")
        .max_total_size(64 * 1024 * 1024)
        .build()
        .expect("build trace writer");
    let recorder = dial9::recorder(writer).build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    let runtime = recorder.handle().attach_tokio_runtime(
        builder,
        TokioAttachOptions::builder()
            .task_tracking_enabled(true)
            .task_dump_config(
                TaskDumpConfig::builder().idle_threshold(Duration::from_millis(10)).build(),
            )
            .build(),
    )?;

    Ok((recorder, runtime))
}

#[dial9::main(config = my_config)]
async fn main() { /* ... */ }
```

> Performance note: Task dumps currently produce one extra wake per capture and are more likely than other features to degrade performance. Measure overhead in your environment before enabling in latency-sensitive paths.

### Custom events

You can emit your own application-level events into the trace alongside the built-in runtime events. Define a struct with `#[derive(TraceEvent)]` and call `record_event`:

```rust,no_run
# fn main() {
use dial9::Dial9Handle;
use dial9::core::clock_monotonic_ns;
use dial9::format::TraceEvent;

#[derive(TraceEvent)]
struct RequestCompleted {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    status_code: u32,
    #[traceevent(unit = "us", kind = "gauge")]
    latency_us: u64,
    /// Optional fields use 1 byte on the wire when absent.
    error_message: Option<String>,
}

let handle = Dial9Handle::current();
handle.record_event(RequestCompleted {
    timestamp_ns: clock_monotonic_ns(),
    status_code: 200,
    latency_us: 1500,
    error_message: None,
});
# }
```

`current()` resolves on any thread a dial9 runtime owns. To record from a synchronous worker thread instead, call `Recorder::install_global()` once at startup: `current()` then resolves there too, and `dial9::record_event` does the same without naming a handle. Such a thread can also opt into sched and CPU sampling with `Dial9Handle::track_current_thread()`.

### Custom event callbacks

You can also register a callback that runs from dial9's flush thread and emits
custom events. This is useful for draining application-owned queues or taking
periodic snapshots without passing a [`Dial9Handle`] through your code:

```rust,no_run
use dial9::core::CustomEventsConfig;
use dial9::format::TraceEvent;
use dial9::{RecorderSourceExt, recorder};

#[derive(TraceEvent)]
struct CacheEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    entries: u64,
}
# let writer = dial9::MemoryBuffer::new(1 << 20).unwrap();
# let (_tx, rx) = std::sync::mpsc::channel::<CacheEvent>();
let recorder = recorder(writer)
    .with_custom_events(CustomEventsConfig::default(), move |ctx| {
        while let Ok(event) = rx.try_recv() {
            ctx.record_event(event);
        }
    })
    .build();
```

`CustomEventsConfig::default()` runs the callback every flush cycle
while telemetry is enabled, which fits drain-style callbacks. For polling-style
callbacks, configure `minimum_interval(...)` to limit how often dial9 invokes
the callback.

### Custom Runtime Hooks

dial9 installs callbacks on all 8 Tokio runtime hooks to collect telemetry. If you need to run your own logic alongside dial9's instrumentation, pass your own `TokioHooks` when attaching:

```rust,no_run
use dial9::{Dial9HandleTokioExt, MemoryBuffer, TokioAttachOptions, TokioHooks, recorder};

let mut hooks = TokioHooks::default();
hooks.on_thread_start(|| {
    println!("Worker thread started");
});
hooks.on_thread_stop(|| {
    println!("Worker thread stopping");
});
// Also available: on_thread_park, on_thread_unpark,
// on_task_spawn, on_task_terminate, on_before_task_poll, on_after_task_poll

let recorder = recorder(MemoryBuffer::new(16 * 1024 * 1024).unwrap()).build();

let mut builder = tokio::runtime::Builder::new_multi_thread();
builder.enable_all().worker_threads(4);
let runtime = recorder
    .handle()
    .attach_tokio_runtime(
        builder,
        TokioAttachOptions::builder().tokio_hooks(hooks).build(),
    )
    .unwrap();
```

dial9's internal hooks always run first, then your callbacks fire in registration order. This ensures `Dial9Handle::current()` is available in your `on_thread_start` callback. Registering the same hook multiple times stacks the callbacks — all of them will fire.

**Important:** Do not set thread or task hooks on the `tokio::runtime::Builder` you hand to `attach_tokio_runtime`; dial9 installs its own and yours would be overwritten. Always go through `TokioHooks` so your callbacks compose with dial9's instrumentation.

## Getting data out of dial9

dial9 is recording data to in memory buffers and eventually to disk. For most applications, they would like the data to go somewhere else. `dial9` has a built in exporter for S3 and it is also possible to write your own exporter.

### Exporting data to S3
dial9 has a built-in S3 exporter. When segments are sealed, symbolized, and compressed they will be uploaded to S3 by a background thread. The `dial9` viewer includes a browser to browse the traces stored on S3.

**Enable the `worker-s3` feature:**
```toml
[dependencies]
dial9 = { version = "0.5", features = ["worker-s3"] }
```

**Create the S3 bucket**: Ensure your application has `s3:PutObject` and `s3:ListBucket` permissions to the bucket.

**Set `with_s3_uploader`:**
```rust,no_run
# #[cfg(feature = "worker-s3")]
# mod inner {
use std::io;

use dial9::s3::S3Config;
use dial9::{AttachedRuntime, Dial9HandleTokioExt, DiskBuffer, RecorderPipelineExt, TokioAttachOptions};

fn my_config() -> io::Result<AttachedRuntime> {
    let s3_config = S3Config::builder()
        .bucket("my-trace-bucket")
        .service_name("my-service")
        .build();

    let writer = DiskBuffer::builder()
        .base_path("/tmp/dial9")
        .max_total_size(1 << 30)
        .build()
        .expect("build trace writer");
    let recorder = dial9::recorder(writer)
        .with_s3_uploader(s3_config)
        .build();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    let runtime = recorder.handle().attach_tokio_runtime(
        builder,
        TokioAttachOptions::builder().task_tracking_enabled(true).build(),
    )?;

    Ok((recorder, runtime))
}

#[dial9::main(config = my_config)]
async fn main() {
    // your async code here
}
// on shutdown: flushes, seals final segment, worker drains remaining to S3
# }
# fn main() {}
```

For custom credentials or AWS SDK settings, defer client construction to the
pipeline worker runtime:

```rust,no_run
use dial9::s3::S3Config;
use dial9::{Dial9HandleTokioExt, DiskBuffer, RecorderS3ClientExt, TokioAttachOptions};
use std::time::Duration;

# fn main() -> std::io::Result<()> {
let writer = DiskBuffer::builder()
    .base_path("/tmp/dial9")
    .max_total_size(1 << 30)
    .build()?;
let s3_config = S3Config::builder()
    .bucket("my-trace-bucket")
    .service_name("my-service")
    .build();
let custom_credentials_provider: aws_sdk_s3::config::Credentials = todo!();
let custom_endpoint = "https://s3.example.com";

let recorder = dial9::recorder(writer)
    .with_s3_uploader_client_future(s3_config, async move {
        let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .credentials_provider(custom_credentials_provider)
            .endpoint_url(custom_endpoint)
            .load()
            .await;
        aws_sdk_s3::Client::new(&sdk_config)
    })
    .build();

let mut builder = tokio::runtime::Builder::new_multi_thread();
builder.enable_all();
let runtime = recorder
    .handle()
    .attach_tokio_runtime(builder, TokioAttachOptions::default())?;

drop(runtime);
recorder.graceful_shutdown(Duration::from_secs(5));
# Ok(())
# }
```

The future is polled when the pipeline worker starts. The resulting client,
including its credential refresh support, remains on the worker runtime.

When you use `#[dial9::main]`, this shutdown drain happens
automatically once `main` returns: the macro drops the runtime, then calls
`graceful_shutdown` with a 1s deadline so the final segment is uploaded. Tune it
with `#[dial9::main(graceful_shutdown = Duration::from_secs(5))]`, or turn it off
with `#[dial9::main(disable_graceful_shutdown)]`. Driving the runtime yourself,
do the same in order: `drop(runtime)` first so its workers flush, then
`recorder.graceful_shutdown(timeout)`.

### Running without disk (in-memory)

To run with **no filesystem dependency** (disk unavailable, read-only, or unwelcome) use `MemoryBuffer`. Encoded segments stay in process memory and are shipped by the same processor pipeline (S3, custom, ...).

```rust,no_run
# #[cfg(feature = "worker-s3")]
# mod inner {
use dial9::s3::S3Config;
use dial9::{MemoryBuffer, RecorderPipelineExt, recorder};

# fn example() -> std::io::Result<()> {
let writer = MemoryBuffer::new(16 * 1024 * 1024)?; // 16 MiB RAM budget

let s3 = S3Config::builder().bucket("my-bucket").service_name("svc").build();
let recorder = recorder(writer)
    .with_custom_pipeline(|p| p.gzip().s3(s3))
    .build();
# let _ = recorder;
# Ok(())
# }
# }
# fn main() {}
```

`max_total_size` bounds the in-memory buffers: if a slow exporter falls behind, the oldest sealed segments are dropped rather than blocking recording. See [`examples/in_memory_pipeline.rs`](https://github.com/dial9-rs/dial9/blob/HEAD/dial9/examples/in_memory_pipeline.rs).

### Exporting data to other destinations

For custom upload destinations or post-processing (e.g. shipping to a different object store, running analysis on each segment), you can replace the built-in pipeline entirely with `with_custom_pipeline`. See [`examples/custom_pipeline.rs`](https://github.com/dial9-rs/dial9/blob/HEAD/dial9/examples/custom_pipeline.rs) for a complete example.

## Analyzing trace files
[`dial9`](https://github.com/dial9-rs/dial9/tree/HEAD/dial9-viewer) is a CLI for browsing and analyzing traces. Use `dial9 serve` to start a local web UI that visualizes traces from a directory or S3 bucket. [Here's a demo.](https://www.youtube.com/watch?v=kr0RYMu57kU)

Pre-built binaries are available from [GitHub Releases](https://github.com/dial9-rs/dial9/releases) for Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows (x86_64).

```bash
# From source via crates.io (the viewer/CLI is behind the `cli` feature)
cargo install --locked dial9 --features cli

# Or install the local-filesystem-only viewer without S3 or the AWS SDK
cargo install --locked dial9-viewer --no-default-features

# Or with cargo-binstall (downloads a pre-built binary, faster)
cargo binstall dial9
```

## Usage

The binary has several subcommands: `serve`, `agents`, `trace-shape`, and `report`. Run `dial9 --help` or `dial9 <subcommand> --help` for full options.

### `serve`

Starts a web server for browsing and viewing traces from S3 or the local filesystem.

```bash
# Serve traces from a local directory
dial9 serve --local-dir /tmp/my_traces

# Serve traces from S3
AWS_PROFILE=my-profile dial9 serve --bucket my-trace-bucket

# Explore the complete browser and aggregation flow without S3
dial9 serve --simulator --local
```

Open `http://localhost:3000` to browse traces. Enter a search prefix (e.g. `2026-04-09/1910/checkout-api`), select one or more segments, and click "View Selected" to open them in the viewer.

#### Simulator mode

Simulator mode exposes lazily generated traces through the same S3-shaped keys
and storage interface as a real trace bucket. Browser discovery, object
downloads, spans, flamegraphs, and Tokio stats therefore use their production
paths, while aggregate rollups stay in a process-local temporary directory.
No bucket or AWS credentials are required.

```bash
# Sanitized synthetic traces with every feature group enabled
dial9 serve --simulator --local

# Replay the bundled demo trace in each virtual segment
dial9 serve --simulator demo --local

# Model a larger fleet with five-minute segments
dial9 serve --simulator --simulator-hosts 12 --simulator-segment-secs 300 --local

# Keep selected synthetic features and repeat the template for more data
dial9 serve --simulator synthetic \
  --simulator-features cpu,scheduling,tasks,spans \
  --simulator-repetitions 3 \
  --simulator-symbols realistic --local
```

The default fleet has 3 hosts and one-minute virtual segments across any
requested time range. The catalog is deterministic and independent of server
uptime; payload bytes are generated only when an object is fetched. Use
`--simulator-hosts`, `--simulator-segment-secs`, and
`--simulator-repetitions` to change its shape and data volume. Synthetic
feature groups are `cpu`, `scheduling`, `tasks`, `spans`, `memory`,
`resources`, and `custom-events`; omit `--simulator-features` to enable all of
them, or pass `none` for clock and segment metadata only. Use
`--simulator-symbols realistic` for deterministic Rust-like stack-frame names;
anonymous placeholders remain the default. Demo replay preserves the bundled
trace's event data while rebasing one copy into every virtual segment.

### `agents`

Manages skill documentation and the JS analysis toolkit for AI agents.

```bash
# Print the agent skill header
dial9 agents

# Print a specific skill segment
dial9 agents skill recipes

# Unpack all skills as an Agent Skills spec directory (for native skill loading)
dial9 agents skills /tmp/dial9-skills

# Extract the JS analysis toolkit to a directory
dial9 agents toolkit /tmp/dial9-toolkit
node /tmp/dial9-toolkit/analyze.js /tmp/my_traces/
```

If you use [Symposium](https://symposium.dev), skills auto-install when your project depends on `dial9`:

```bash
cargo agents sync
```

### `trace-shape`

Extracts sanitized structural fingerprints ("shapes") from traces, or generates
synthetic traces from shapes. Useful for sharing trace structure with raw
payloads, labels, and identifiers removed.

```bash
# Sanitize directly into a synthetic trace, bypassing shape JSON (recommended for large traces)
dial9 trace-shape synthesize /tmp/traces/trace.bin synthetic.bin --repeat 3

# Extract a portable shape (accepts gzip trace input)
dial9 trace-shape extract /tmp/traces/trace.bin shape.json

# Generate a synthetic trace from a previously extracted shape
dial9 trace-shape generate shape.json synthetic.bin --repeat 3
```

The `synthesize` operation keeps the sanitized replay template in memory and
writes the synthetic binary directly. It uses the same validation and privacy
transformations as the two-step workflow, but does not serialize or reparse the
verbose per-event JSON representation.

**Privacy caveat:** Shape extraction applies deterministic transformations to
remove string contents, byte payloads, custom names, and exact timestamps. Small
structural integers (e.g. `worker_id`, task counts) are intentionally preserved.
This is **not an anonymization or security boundary**. Exact booleans, small
quantized integers, and already-round floats survive. Shapes intentionally
**retain sensitive operational structure** including relative timing, event
ordering, cardinality, byte payload sizes, stack depths, value magnitude
distributions, and inter-event correlations. Synthetic traces should be treated
as confidential operational data.

## License

This project is licensed under the Apache-2.0 License.
