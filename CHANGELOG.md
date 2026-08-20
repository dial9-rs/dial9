# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- dial9 now builds without `--cfg tokio_unstable` ([#364](https://github.com/dial9-rs/dial9/issues/364)).
  With the flag nothing changes. Without it, poll events come from dial9's future wrapper instead of tokio's hooks, so they cover tasks spawned through `dial9::spawn`, `spawn_in`, `block_on` and `spawn_with` rather than every task on the runtime.
  Task spawn/terminate events and per-worker queue depth have no stable source and are unavailable. 
  Traces carry `tokio.poll_coverage` and `tokio.local_queue` metadata keys describing what the trace holds, plus `tokio.unstable` for how it was built, and the viewer reports the reduced coverage.
  The `taskdump` feature still requires the flag: it forwards to `tokio/taskdump`, which is a hard compile error without it.
- `JoinSetExt` adds dial9-instrumented `spawn_traced` and `spawn_traced_on`
  methods to Tokio `JoinSet`s while preserving caller locations.

### Changed

- **Breaking:** the default S3 uploader key layout now uses ordered Hive-style
  partitions (`version=1/date=…/service=…/time=…/instance=…/boot=…`) with Hive
  path escaping. The viewer reads both the new and historical layouts; custom
  `S3KeyFn` output is unchanged ([#789](https://github.com/dial9-rs/dial9/issues/789)).
- **Breaking:** `dial9-viewer` exposes its S3 APIs through a default-on `s3`
  feature. The `dial9` crate keeps its empty default feature set, while `cli`
  retains the existing S3-enabled binary. For a local-only viewer without S3
  or the AWS SDK, use `dial9-viewer` directly with its default features
  disabled ([#722](https://github.com/dial9-rs/dial9/pull/722)).

## [0.5.0-rc2](https://github.com/dial9-rs/dial9/compare/dial9-v0.5.0-rc1...dial9-v0.5.0-rc2) - 2026-08-03

Two crates split out from dial9. `dial9-metrique` records
[metrique](https://docs.rs/metrique) unit-of-work entries into the trace, and
`dial9-destinations-s3` owns S3 upload, now reachable as `dial9::s3`. Both arrive
through `dial9` features (`metrique-sink`, `worker-s3`), so there is nothing new
to depend on.

Tokio attach moved from `Recorder` to `Dial9Handle`, so several threads can clone
one handle and each build and attach their own runtime. `dial9` also re-exports
the event-authoring and decode surface from `dial9-trace-format`, so custom
events no longer need a second dependency.

### Breaking

- Tokio attach moved to `Dial9Handle`. `RecorderTokioExt` and its `Recorder::attach_tokio_runtime` / `attach_tokio_runtime_with` are gone; `Dial9HandleTokioExt::attach_tokio_runtime(builder, options)` is the only way to attach ([#732](https://github.com/dial9-rs/dial9/pull/732)).

  Attach borrows the handle instead of consuming the recorder, so services can clone one handle and let each thread build and attach its own runtime. You now build the Tokio builder yourself, which also means calling `enable_all()` (previously implicit) and picking the flavor.

  ```rust
  let recorder = dial9::recorder(writer).build();

  let mut builder = tokio::runtime::Builder::new_multi_thread();
  builder.enable_all().worker_threads(4);
  let runtime = recorder.handle().attach_tokio_runtime(
      builder,
      TokioAttachOptions::builder().runtime_name("api").build(),
  )?;
  ```

  Attaching after `graceful_shutdown` returns an error, and surviving handles go inert: `is_enabled` reports false and `enable` is a no-op.
- `dial9-core`: `ThreadLocalEncoder::write_event` returns `io::Result<()>` instead of panicking the calling thread on a validation failure; the event is dropped and callers own reporting ([#723](https://github.com/dial9-rs/dial9/pull/723)).
- `dial9-core`: `Dial9Handle::is_enabled` now reports whether recording currently does anything (connected AND not paused), not just connectedness ([#723](https://github.com/dial9-rs/dial9/pull/723)).

### Added

- Metrique sink: dial9 can record [metrique](https://docs.rs/metrique) unit-of-work entries into the trace as a peer of an existing EMF/JSON pipeline, with per-request thread/task/timing context. New tokio-free `dial9-metrique` crate, re-exported as `dial9::metrique_sink` behind the `metrique-sink` feature; combine with the `tokio` feature to capture task ids ([#189](https://github.com/dial9-rs/dial9/issues/189), [#723](https://github.com/dial9-rs/dial9/pull/723)).

  ```rust
  use dial9::metrique_sink::{Dial9Context, Dial9Stream};

  #[metrics(rename_all = "PascalCase")]
  struct RequestMetrics {
      // Including a Dial9Context opts this entry into the trace.
      #[metrics(flatten)]
      dial9: Dial9Context,
      #[metrics(flags(dial9::Interned))]
      operation: &'static str,
      latency_ms: u64,
  }

  // Tee dial9 alongside the existing pipeline; `dial9.*` context fields
  // stay out of the EMF output.
  let _join = ServiceMetrics::attach_to_stream(Dial9Stream::tee(&handle, emf_stream));

  let mut m = RequestMetrics {
      dial9: Dial9Context::capture(),
      operation: "GetPet",
      latency_ms: 0,
  }
  .append_on_drop(ServiceMetrics::sink());
  ```

  For entries you cannot (or would rather not) add a field to, `append_on_drop_dial9` attaches the same context from the outside:

  ```rust
  use dial9::metrique_sink::Dial9EntryExt;

  let mut m = RequestMetrics { operation: "GetPet", latency_ms: 0 }
      .append_on_drop_dial9(ServiceMetrics::sink());
  ```
- *(metrique)* single-event span format (packed end + span.duration role) ([#736](https://github.com/dial9-rs/dial9/pull/736))
- re-export dial9-trace-format's event-authoring and decode surface ([#734](https://github.com/dial9-rs/dial9/pull/734))
- Axum integrations for `dial9-utils` ([#696](https://github.com/dial9-rs/dial9/pull/696))
- *(s3)* enable async S3 client construction ([#702](https://github.com/dial9-rs/dial9/pull/702))
- *(viewer)* dynamic field chart panels ([#741](https://github.com/dial9-rs/dial9/pull/741))
- *(viewer)* add virtual trace simulator ([#738](https://github.com/dial9-rs/dial9/pull/738))
- *(viewer)* Tokio Stats "Scheduling Delay" rollup + focus deep-link ([#715](https://github.com/dial9-rs/dial9/pull/715))
- *(viewer)* add privacy-preserving session measurement ([#726](https://github.com/dial9-rs/dial9/pull/726))

### Changed

- S3 upload lives at `dial9::s3` (was `dial9::core::pipeline::s3`), from a new `dial9-destinations-s3` crate. The `worker-s3` feature is unchanged ([#742](https://github.com/dial9-rs/dial9/pull/742)).

### Fixed

- *(docs)* point manual attach at dial9::block_on ([#739](https://github.com/dial9-rs/dial9/pull/739))
- *(core)* implement Debug for Recorder, FlushContext, and perf ([#727](https://github.com/dial9-rs/dial9/pull/727))
- *(release)* build the dial9 bin with its feature, fix Windows paths ([#717](https://github.com/dial9-rs/dial9/pull/717))

### Other

- *(trace-format)* identity fast path for repeated dynamic Schema handles ([#729](https://github.com/dial9-rs/dial9/pull/729))
- document segment metadata configuration ([#731](https://github.com/dial9-rs/dial9/pull/731))
- add dial9-utils readme ([#747](https://github.com/dial9-rs/dial9/pull/747))
- *(metrique)* drop version from tokio-telemetry dev-dep ([#750](https://github.com/dial9-rs/dial9/pull/750))


## [0.5.0-rc1](https://github.com/dial9-rs/dial9/compare/dial9-v0.5.0-rc0...dial9-v0.5.0-rc1) - 2026-07-24

### Other

- make viewer/cli feature off by default & update changelog ([#712](https://github.com/dial9-rs/dial9/pull/712))

## [0.5.0-rc0](https://github.com/dial9-rs/dial9/compare/dial9-tokio-telemetry-v0.5.0-rc0...dial9-tokio-telemetry-v0.5.0-rc0) - 2026-07-23

`dial9` is now the facade for all dial9 features: one dependency, `dial9 = "0.5"`, re-exporting the recorder, the
Tokio instrumentation, the perf sources, and the viewer CLI, and owning `#[dial9::main]` and
`dial9::record_event`. This is a breaking release: you depend on `dial9` instead of
`dial9-tokio-telemetry`, and the config, writer, and handle APIs all change.

`Dial9Config` is gone. You build a writer, wrap it in `dial9::recorder(writer)`, chain sources,
and build a `Recorder`. Tokio instrumentation is one more source on it.

```rust
let writer = DiskBuffer::builder().base_path("/tmp/dial9-traces").build()?;
```

Under `#[dial9::main]`, that is the whole setup: `attach_tokio_runtime` builds the instrumented runtime and
hands back both, so `config` stays a single expression.

```rust
#[dial9::main(config = || dial9::recorder(writer)
    .with_cpu_profiling(CpuProfilingConfig::default())
    .build()
    .attach_tokio_runtime(|t| { t.worker_threads(4); }))]
async fn main() { /* ... */ }
```

Each `attach_tokio_runtime` attaches another runtime and returns the recorder, so the next call chains on. Attaching two looks like attaching one:

```rust
let recorder = dial9::recorder(writer)
    .with_cpu_profiling(CpuProfilingConfig::default())
    .build();

let (recorder, api_rt) = recorder.attach_tokio_runtime_with(
    TokioAttachOptions::builder().runtime_name("api").build(),
    |t| { t.worker_threads(4); },
)?;
let (recorder, io_rt) = recorder.attach_tokio_runtime_with(
    TokioAttachOptions::builder().runtime_name("io").build(),
    |t| { t.worker_threads(2); },
)?;

// Drop the runtimes before draining, so their workers flush.
drop(api_rt);
drop(io_rt);
recorder.graceful_shutdown(Duration::from_secs(5));
```

Without Tokio, a `Recorder` records on its own and any `Source` plugs in:

```rust
let recorder = dial9::recorder(writer)
    .with_process_resource_usage(ProcessResourceUsageConfig::default())
    .build();
recorder.handle().record_event(MyEvent { .. });
```

`build()` starts recording. Chain `.paused()` before it to build a quiet recorder and
start it later with `Recorder::enable()`; `build_and_start()` is gone, since `build()` is
what it did.

Per-source knobs (`.with_cpu_profiling`, `.with_memory_profiling`, …) and the pipeline overrides
(`.with_custom_pipeline`, `.with_s3_uploader`) live on the recorder builder.
Per-runtime settings (runtime name, task tracking, task dumps, custom hooks) live in `TokioAttachOptions`.
`dial9::recorder_from_env()` builds a recorder and its runtime from the unchanged `DIAL9_*` vars. `dial9::recorder_or_disabled(writer)` starts a builder that downgrades to a disabled recorder when the writer cannot be created, so sources and a pipeline still chain onto it and a bad trace path costs telemetry rather than the process. `dial9::recorder_disabled()` covers telemetry-off.

`TracedRuntime`, the low-level `TracedRuntime::builder()`, and the pipeline / trace-path type-state markers are gone.

The refactor also renamed the trace writers to buffers: `RotatingWriter` → `DiskBuffer`, with
`DiskBuffer` / `MemoryBuffer` the public storage backends and a fully in-memory pipeline that needs
no filesystem (`SegmentData::segment()` now returns `&SegmentRef`). It collapsed the handles:
`TelemetryHandle` / `RuntimeTelemetryHandle` into `Dial9Handle` for record/control and
`Dial9TokioHandle` for spawn, so `record_event(event, &handle)` becomes `handle.record_event(event)`.
And it moved the non-Tokio pieces (custom events, symbolization, process-resource and socket sources,
memory profiling) into `dial9-core` / `dial9-perf-self-profile`, with their public APIs
(`Dial9Allocator`, `MemoryProfiler`, …) unchanged.

Custom processors gain `SegmentProcessor::finalize_dump` and `ProcessError::into_parts`.

Profiling features (`cpu-profiling`, `memory-profiling`, `process-resource`, `linux-socket`) are
now standalone sources that no longer pull in `tokio`: they auto-wire when `tokio` is on. The
facade ships no default features, so a `dial9` library dependency pulls no viewer or CLI weight;
install the CLI with `cargo install dial9 --features cli`.

### Migrating from 0.3

| You had | You now write |
| --- | --- |
| `dial9-tokio-telemetry` dependency | `dial9` (enable the `tokio` feature) |
| `#[dial9_tokio_telemetry::main]` | `#[dial9::main]` |
| `Dial9Config::builder()…build()` | `dial9::recorder(writer).attach_tokio_runtime(..)` (build the writer first) |
| `.on_disk_buffer(p)` / `.in_memory_buffer()` | `DiskBuffer::builder().base_path(p)…build()` / `MemoryBuffer::new(cap)` |
| `Dial9Config::from_env()` | `dial9::recorder_from_env()` |
| `TelemetryHandle` / `RuntimeTelemetryHandle` | `Dial9Handle` (spawn via `Dial9TokioHandle`) |
| `record_event(event, &handle)` | `handle.record_event(event)` |
| `build_and_start()` | `build()` (`.paused()` to opt out) |

Process resource usage (rusage) sampling is now opt-in behind the `process-resource` feature. It was on by default on Unix, so Unix users stop getting rss / page-fault events until they enable it.

### Added

- Tokio as a source: `recorder.attach_tokio_runtime(|t| ..)` builds a Tokio runtime instrumented against a plain `Recorder` and returns both, with `attach_tokio_runtime_with(opts, |t| ..)` for per-runtime settings. It hands the recorder back, so calling it again attaches another runtime and a single and a multi-runtime setup are the same code. `dial9::spawn_in(&runtime, future)` spawns an instrumented task onto a specific runtime from any thread ([#356](https://github.com/dial9-rs/dial9/issues/356))
- `dial9::block_on(&runtime, future)` runs a future to completion on `runtime` with the future itself instrumented. Plain `Runtime::block_on` polls outside any task, so the root future's polls and wakes are not recorded
- `RecorderPipelineExt` on the recorder builder for departing from the default segment pipeline: `.with_custom_pipeline(..)`, `.with_s3_uploader(cfg)` / `.with_s3_uploader_client(cfg, client)`. The default pipeline itself needs no opt-in, and the S3 uploader may now be set before or after the sources whose data it symbolizes
- `Source::segment_processor` lets a source contribute a stage to the default pipeline. The CPU profiler uses it to pull in symbolization, so a trace with stack samples is symbolized without the caller wiring anything up ([#356](https://github.com/dial9-rs/dial9/issues/356))
- Explicit CPU profiling backend selection via `CpuProfilingConfig::with_perf_backend()` and `CpuProfilingConfig::with_ctimer_backend()` constructors. The default (Auto: try perf, fall back to ctimer) is unchanged ([#579](https://github.com/dial9-rs/dial9/issues/579), [#660](https://github.com/dial9-rs/dial9/pull/660))
- `#[dial9::main]` now performs an implicit graceful shutdown after the async body returns: it drops the runtime and drains the background worker so the final segment is symbolized, compressed, and uploaded. Configure the deadline with `#[dial9::main(graceful_shutdown = Duration::from_secs(5))]` (default 1s) or skip it with `#[dial9::main(disable_graceful_shutdown)]`. Without the macro, drive it yourself: drop the runtime, then `Recorder::graceful_shutdown(timeout)` ([#479](https://github.com/dial9-rs/dial9/issues/479))
- `dial9::AttachedRuntime`, the `(Recorder, tokio::runtime::Runtime)` pair `attach_tokio_runtime` hands back. A `#[dial9::main]` config is any zero-argument function returning `std::io::Result<AttachedRuntime>`, so it can be a single expression; the macro panics on `Err` ([#356](https://github.com/dial9-rs/dial9/issues/356))
- `dial9::recorder_from_env_with(|t| ..)` is `recorder_from_env` plus control over the Tokio runtime it builds, so an env-configured service can still set worker counts and thread names. Both return `std::io::Result<AttachedRuntime>`
- `Dial9Handle::track_current_thread()` starts per-thread profiling of the calling thread and returns a guard that stops it on drop. Sources that sample per thread, such as the scheduler-event profiler, only see threads that opt in; Tokio workers do it themselves, so this is what makes them usable from a plain `std::thread` or a non-Tokio program

### Changed

- Programs using `#[dial9::main]` no longer exit immediately: clean exit now blocks up to the graceful-shutdown deadline (default 1s) to drain the final segment, a bounded amount of added latency (see Added for the config and opt-out) ([#479](https://github.com/dial9-rs/dial9/issues/479))
- Worker IDs are now reserved when a runtime's workers first poll, rather than at attach time (the caller builds the runtime, so there is nothing to count when hooks are registered). A runtime's `runtime.{name}` → worker-ID mapping therefore appears in segment metadata once that runtime has run work, and the ID blocks follow worker start-up order rather than attach order ([#356](https://github.com/dial9-rs/dial9/issues/356))
- **Breaking:** `Recorder::graceful_shutdown` returns `()` instead of an always-`Ok` `io::Result<()>`. Drain failures are logged, as they already were
- **Breaking:** `Source` gains an `Any` supertrait, so a source must be `'static` (boxed sources already were). This lets a layer recover its own concrete source from the recorder, which is how a second `attach_tokio_runtime` finds the runtime registry the first one installed
- **Breaking:** `RecorderBuilder::build()` starts recording; `build_and_start()` is removed and `.paused()` opts out
- **Breaking:** `Source::on_worker_thread_start` is renamed to `Source::on_thread_start`. It fires whenever a thread joins the recorder, which is no longer only a Tokio worker's first poll
- **Breaking:** `boot_id` is no longer an `S3Config` builder field. The runtime injects the on-disk namespace `boot_id` into the S3 config at build time, so a local trace segment and its S3 key share one identity. An `S3Config` built outside the managed `recorder_from_env` path falls back to a fresh `{4-alpha}-{pid}` ([#566](https://github.com/dial9-rs/dial9/pull/566))

### Fixed

- Viewer: browsing a nonexistent bucket now returns HTTP 404 instead of 500, and a syntactically invalid bucket name returns HTTP 400. The S3 `NoSuchBucket`/`NoSuchKey` and `InvalidBucketName` error codes were falling through to the generic error arm, which logged an "unclassified S3 error" and reported a server `fault` — so a user typo in the bucket name polluted the viewer's fault metric. They now classify as `NotFound` (404) and `BadRequest` (400) respectively.

### Viewer

The trace viewer is rebuilt as a Vite + TypeScript app. The trace decoder and parser are unchanged (a frozen core), so the wire format and decoding behavior are identical.

New:

- Shareable URL state. Viewport, selection, canvas selections, issues-rail, span filters, and span focus all ride the URL, so a view links and reproduces exactly.
- Search palette. `/` opens a search over tasks, spans, and points of interest.
- Inspector sidebar. Tabbed, with poll detail, a related view, and an embedded region-analysis panel.
- New tracks: worker lanes, CPU usage, queue depth, spans, task detail, and custom events, each with hover, click-to-pin, and a legend.
- Per-track collapse and drag-reorder, persisted across sessions, with animated collapse.
- Overview minimap and status bar, plus a responsive toolbar showing trace service and host.
- Flamegraph inspect and diff modes, with a histogram and minimap.
- Tokio Stats worker-activity rollup with focus deep-linking.
- Large-trace handling. Columnar events and a main-thread loader remove the structured-clone OOM. Viewport scans are bounded, Set/Clear Range reparses instead of re-rendering, and POI counts are capped.
- Keyboard shortcuts. Press `?` in the viewer for the list.

Under the hood, all four pages (viewer, browser, flamegraph, tokio_stats) move from hand-written HTML/JS to typed modules under `dial9-viewer/ui/src`, bundled to `dist/` and embedded via rust-embed as before. The port is guarded by a parity harness and a Vitest suite, with an import-boundary check keeping pages out of the frozen core. The browser page also reaches WCAG AA contrast.

### Other

- [**breaking**] tokio instrumentation as a source ([#698](https://github.com/dial9-rs/dial9/pull/698))
- make dial9 the facade ([#614](https://github.com/dial9-rs/dial9/pull/614))
- move memory profiling to dial9-perf-self-profile ([#591](https://github.com/dial9-rs/dial9/pull/591))
- move event bus to dial9-core ([#549](https://github.com/dial9-rs/dial9/pull/549))
- setup dial9-core ([#540](https://github.com/dial9-rs/dial9/pull/540))
- Drop aws-sdk-s3-transfer-manager, upload segments via aws-sdk-s3 PutObject ([#668](https://github.com/dial9-rs/dial9/pull/668))
- *(deps)* bump s3s crates to 0.14.1 to pull in patched quick-xml ([#611](https://github.com/dial9-rs/dial9/pull/611))

## [0.3.13](https://github.com/dial9-rs/dial9/compare/dial9-tokio-telemetry-v0.3.12...dial9-tokio-telemetry-v0.3.13) - 2026-05-29

### Added

- Add resource usage events to trace (rss, page faults etc.) ([#470](https://github.com/dial9-rs/dial9/pull/470))

## [0.3.12](https://github.com/dial9-rs/dial9/compare/dial9-tokio-telemetry-v0.3.11...dial9-tokio-telemetry-v0.3.12) - 2026-05-28

This release adds **memory profiling** and **CPU-profiler-only mode**.

### Memory Profiling ([#442](https://github.com/dial9-rs/dial9/pull/442), [#443](https://github.com/dial9-rs/dial9/pull/443), [#452](https://github.com/dial9-rs/dial9/pull/452), [#459](https://github.com/dial9-rs/dial9/pull/459))

dial9 can now sample heap allocations and produce allocation flamegraphs. With liveset tracking enabled, you can see which allocations are never freed.

```rust
use dial9_tokio_telemetry::memory_profiling::{
    Dial9Allocator, MemoryProfiler, MemoryProfilingConfig,
};

#[global_allocator]
static ALLOC: Dial9Allocator = Dial9Allocator::system();

let config = MemoryProfilingConfig::builder()
    .sample_rate_bytes(512 * 1024)  // sample ~every 512 KiB allocated
    .track_liveset(true)            // track frees for leak detection
    .build();

let _guard = MemoryProfiler::from_config(config)
    .install(handle)
    .expect("failed to install memory profiler");
```

The viewer includes a new **heap flamegraph** tab with toggleable bytes/count views. The trace emits `MemoryProfileOverflowEvent` when ring buffers overflow so you know when leak counts may be inflated.

### CPU Profiler Only Mode ([#454](https://github.com/dial9-rs/dial9/pull/454))

dial9 can now be used purely as a CPU profiler without Tokio runtime hooks. Useful for non-Tokio applications or when you only need flamegraphs:

```rust
let (runtime, guard) = TracedRuntime::builder()
    .with_cpu_profiling(CpuProfilingConfig::default())
    .with_tokio_instrumentation(false)
    .build_and_start(tokio::runtime::Builder::new_multi_thread(), writer)?;
```

Or via environment variables:

```text
DIAL9_ENABLED=true
DIAL9_CPU_PROFILE_ENABLED=true
DIAL9_TOKIO_INSTRUMENTATION_ENABLED=false
```

### Setup Diagnostics Skill ([#464](https://github.com/dial9-rs/dial9/pull/464))

The agent toolkit now runs setup diagnostics before analysis, detecting missing frame pointers, uninstrumented tasks, stripped debug symbols, and disabled scheduling events. When detected, it provides instructions to fix:

```
🔴 [missing-frame-pointers] CPU stack traces are only 1.4 frames deep on average (expected 10+).

  Fix: Add to .cargo/config.toml:
  [build]
  rustflags = ["--cfg", "tokio_unstable", "-C", "force-frame-pointers=yes"]

🟡 [missing-wake-events] 50 tasks spawned but 0 wake events recorded.

  Fix: Use TelemetryHandle::spawn() instead of tokio::spawn()
```

### Other Changes

- *(tokio-telemetry)* Add `Deserialize` to built-in event structs ([#451](https://github.com/dial9-rs/dial9/pull/451), [#447](https://github.com/dial9-rs/dial9/pull/447))
- *(viewer)* Continuous log-scale poll color heatmap replaces the old 4-bucket scheme ([#453](https://github.com/dial9-rs/dial9/pull/453))
- `max_file_size` is now optional in `Dial9Config` — defaults to `min(100 MiB, max_total_size / 4)` ([#456](https://github.com/dial9-rs/dial9/pull/456))
- Rotation period is now measured monotonically, removing wallclock-time edge cases ([#461](https://github.com/dial9-rs/dial9/pull/461))
- Trace metadata now includes `dial9.version` for debugging version mismatches ([#463](https://github.com/dial9-rs/dial9/pull/463))
- Viewer: allow empty prefix when searching S3 buckets ([#460](https://github.com/dial9-rs/dial9/pull/460))

## [0.3.11](https://github.com/dial9-rs/dial9/compare/dial9-tokio-telemetry-v0.3.10...dial9-tokio-telemetry-v0.3.11) - 2026-05-22

### Added

- Users can now provide their own Tokio runtime hooks which compose with dial9's. ([#297](https://github.com/dial9-rs/dial9/pull/297)) ([#439](https://github.com/dial9-rs/dial9/pull/439))
- Clients can now be configured with `from_env`, a standard set of environment variables to configure clients ([#406](https://github.com/dial9-rs/dial9/pull/406))
- *(viewer)* add custom events view ([#438](https://github.com/dial9-rs/dial9/pull/438))

### Fixed

- `block_in_place` no longer causes nonsense data in trace files: detect block_in_place gaps and correct CPU sample worker attribution ([#436](https://github.com/dial9-rs/dial9/pull/436))
- enforce RotatingWriter retention across restarts ([#414](https://github.com/dial9-rs/dial9/pull/414))
- *(viewer)* correct KSD navigation time calculation ([#422](https://github.com/dial9-rs/dial9/pull/422)) ([#432](https://github.com/dial9-rs/dial9/pull/432))

### Other

- refactor: inline EventWriter, delete the shallow wrapper ([#434](https://github.com/dial9-rs/dial9/pull/434))
- refactor: split recorder/mod.rs into focused modules ([#433](https://github.com/dial9-rs/dial9/pull/433))
- extract sampling primitives into shared module ([#418](https://github.com/dial9-rs/dial9/pull/418))
- Extract Source trait for flush-thread data sources ([#408](https://github.com/dial9-rs/dial9/pull/408))
- *(design)* in-memory pipeline ([#389](https://github.com/dial9-rs/dial9/pull/389))
- Add connection-established / closed events to the demo trace ([#441](https://github.com/dial9-rs/dial9/pull/441))

## [0.3.10](https://github.com/dial9-rs/dial9/compare/dial9-tokio-telemetry-v0.3.9...dial9-tokio-telemetry-v0.3.10) - 2026-05-15

### Added

- add tid to WorkerParkEvent and WorkerUnparkEvent ([#410](https://github.com/dial9-rs/dial9/pull/410))

### Other

- expose public Unwinder::capture API ([#396](https://github.com/dial9-rs/dial9/pull/396))

## [0.3.9](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.8...dial9-tokio-telemetry-v0.3.9) - 2026-05-14

### Added

- Instrumented JoinSets and other custom spawns via `spawn_with` ([#392](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/392))
- Android schedstat support ([#395](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/395)) — thanks @nickrobinson!

### Fixed

- Recover from missing `.active` file during rotation ([#399](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/399)): if the active trace file or parent directory is removed externally, the writer now recovers gracefully instead of busy-looping.
- Bring back old API on core telemetry builder ([#401](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/401))
- Rate-limit log when drain is failing ([#385](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/385))
- *(toolkit)* Don't pass directory progress callbacks for single-file analyzeTraces ([#384](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/384))
- *(test)* Make `test_schedstat_fd_closed_on_thread_exit` not flaky ([#398](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/398))
- Install pipeline when CPU profiling is enabled ([#404](https://github.com/dial9-rs/dial9/pull/404))

### Other

- Symposium cleanup ([#394](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/394))
- Update README ([#391](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/391))


## [0.3.8](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.7...dial9-tokio-telemetry-v0.3.8) - 2026-05-08

### Added

- Add task dump capture behind `taskdump` feature ([#354](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/354))
- Task dumps: switch to Poisson sampling and libunwind ([#369](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/369))
- Taskdump viewer and expand inline frames in flamegraphs ([#378](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/378))
- Expose runtime pipeline ([#355](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/355), [#365](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/365))
- Add typed list and map FieldTypes ([#367](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/367))
- Add TAG_SCHEMA_ANNOTATIONS frame and SchemaEntry::annotations ([#366](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/366))
- *(viewer)* Adopt agent skills spec, Symposium integration, lightweight benchmark ([#370](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/370))
- *(toolkit)* Task dumps in recipes, bugfixes ([#380](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/380))
- Document task dumps, other README improvements ([#379](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/379))

### Fixed

- Use real waker in task dump capture to prevent lost wakes ([#372](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/372))
- Eliminate task dump busy loop and move dl_iterate_phdr off hot path ([#375](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/375))
- Redo tracing UI and add span close events ([#342](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/342))

### Other

- *(design)* metrique to dial9 integration ([#346](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/346))
- Memory profiling design ([#362](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/362))
- Add iai-callgrind PR gate, retire criterion CI ([#360](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/360))
- Write dial9 crate README ([#374](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/374))
- Add symposium keyword to dial9-tokio-telemetry ([#376](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/376))
- update Cargo.lock dependencies

## [0.3.7](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.6...dial9-tokio-telemetry-v0.3.7) - 2026-05-04

### Added

- Include CPU id in CPU profile samples ([#338](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/338))
- **New config API** ([#256](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/256)): `Dial9Config::builder()` replaces the positional `Dial9ConfigBuilder::new(path, file_size, total_size)` with a fluent builder. Inline closures are now supported in the macro, and `build_or_disabled()` gracefully falls back to a plain tokio runtime on config/IO failure:

  ```rust
  #[dial9_tokio_telemetry::main(config = || {
      Dial9Config::builder()
          .base_path("/tmp/trace.bin")
          .max_file_size(64 * 1024 * 1024)
          .max_total_size(256 * 1024 * 1024)
          .build_or_disabled()
  })]
  async fn main() { /* ... */ }
  ```
- free `dial9_tokio_telemetry::spawn()` function ([#343](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/343))

### Changed

- `TelemetryHandle::current()` no longer panics off-runtime — it returns an inert handle whose `spawn` falls through to `tokio::spawn`. Use `TelemetryHandle::is_enabled()` to check whether telemetry is live. ([#256](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/256))

### Fixed

- fix security audit ([#344](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/344))
- Avoid constructing events when telemetry is disabled ([#332](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/332))
- align span panel to worker lane coordinate system ([#341](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/341))

### Other

- Add metrics section to the prod use docs ([#352](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/352))
- Add SpanCloseEvent to tracing layer ([#348](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/348))
- Remove RawEvent and unify internals to use public API ([#339](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/339))
- fix unresolved intra-doc links in rustdoc builds ([#347](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/347))
- Add dial9-in-prod example ([#335](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/335))

## [0.3.6](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.5...dial9-tokio-telemetry-v0.3.6) - 2026-04-30

### Added

- Store S3 metadata into segement metadata ([#311](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/311))
- detect uninstrumented task spawns and surface in viewer ([#293](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/293))
- Add simple example for local execution ([#306](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/306)) — thanks @mox692!

### Fixed

- Don't register sched events on blocking pool threads ([#316](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/316))
- *(viewer)* correct schedWait unit from µs to ns ([#308](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/308))

### Other

- Fix thread CPU time measurement details in README ([#312](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/312))
- Fix ctimer test on AL2 ([#317](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/317))
- Allow opening .gz trace files in the file picker ([#315](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/315))
- [dial9-viewer] Toolkit: parallel multi-file trace analysis with caching ([#298](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/298))
- Retain parent stack trace when zooming into flamegraph frames ([#305](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/305))
- Retain selection overlay while sidebar is open ([#304](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/304))
- (viewer) Move flamegraph into sidebar instead of full-screen overlay ([#291](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/291))

## [0.3.5](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.4...dial9-tokio-telemetry-v0.3.5) - 2026-04-24

### Added

- *(viewer)* resizable sidebar and slightly improved ux ([#290](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/290))

### Fixed

- *(ci)* use single cargo package invocation for cross-crate verification

### Other

- lower expected sample threshold in ctimer cpu load test ([#292](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/292))
- Restore collect_files safety limits lost by merge-queue bug ([#276](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/276)) ([#295](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/295))
- restore ([#286](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/286))
- Fix stack overflow on large profiles ([#285](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/285))

## [0.3.4](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.3...dial9-tokio-telemetry-v0.3.4) - 2026-04-23

### Added

- add CPU profiling fallback for perf-restricted environments. This should enable CPU profiling to work in Fargate. ([#250](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/250))
- *(viewer)* replace stack trace popup with right sidebar panel ([#274](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/274))
- *(viewer)* Pop-out flamegraph with interactive features ([#269](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/269))

### Fixed

- fix sort order of polls with cpu samples ([#272](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/272))

### Other

- Make worker lanes scrollable in viewer ([#275](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/275))
- Fix docs.rs broken links by using absolute GitHub URLs ([#277](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/277))

## [0.3.3](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.2...dial9-tokio-telemetry-v0.3.3) - 2026-04-20

### Other

- tighten README prose for readability and conciseness ([#265](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/265))

## [0.3.2](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.1...dial9-tokio-telemetry-v0.3.2) - 2026-04-20

### Other

- crosslink dial9-viewer from the readme ([#262](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/262))

## [0.3.1](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.0...dial9-tokio-telemetry-v0.3.1) - 2026-04-19

### Added

- **Tracing layer** ([#252](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/252)): `Dial9TokioLayer` records `tracing` span enter/exit events, including field values, into the trace, showing what happened inside each poll. Enable with the `tracing-layer` feature flag.

The viewer (`dial9-viewer serve`) shows spans in a dedicated panel with filtering, percentile ranking, and click-to-highlight. The agent analysis toolkit (`dial9-viewer agents`) includes span correlation recipes and automated span checks in the red-flags scan.

```rust,ignore
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use tracing_subscriber::prelude::*;

tracing_subscriber::registry()
    .with(Dial9TokioLayer::new().with_filter(
        tracing_subscriber::filter::Targets::new()
            .with_target("my_app", tracing::Level::TRACE)
            .with_default(tracing::Level::ERROR),
    ))
    .init();
```

Tracing support means you can attach a request ID or other context to spans via `#[instrument(fields(request_id = %id))]` and then search for specific requests in the trace. You can also see what's happening inside long polls: if a single poll contains many small operations without yielding, the span breakdown shows exactly where the time went.

Standard `tracing-subscriber` filtering rules apply. Without a filter, libraries like the AWS SDK will flood the trace with internal spans. The preceding captures only spans from `my_app`.

## [0.3.0](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.2.0...dial9-tokio-telemetry-v0.3.0) - 2026-04-17

Big release. The setup story is much better, there's support for tracing multiple runtimes, you can emit your own events into the trace, and the viewer is its own crate now.

### `#[dial9_tokio_telemetry::main]` macro ([#212](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/212))

Drop-in replacement for `#[tokio::main]`. Point it at a config function and you're done. Unlike `#[tokio::main]`, the macro spawns your function body as a task, so top-level code shows up in traces.

```rust
use dial9_tokio_telemetry::config::{Dial9Config, Dial9ConfigBuilder};
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

fn my_config() -> Dial9Config {
    Dial9ConfigBuilder::new("trace.bin", 64 * 1024 * 1024, 256 * 1024 * 1024)
        .with_runtime(|r| r.with_task_tracking(true))
        .build()
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    let handle = TelemetryHandle::current();
    handle.spawn(async { /* wake events tracked */ }).await.unwrap();
}
```

### Multiple runtime support ([#141](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/141), [#193](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/193))

If you run separate runtimes for request handling and background IO (or thread-per-core, etc.), you can now attach them all to one telemetry session with `TelemetryCore`. Workers are grouped by runtime name in the viewer.

```rust
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TelemetryCore};

let writer = RotatingWriter::builder()
    .base_path("/tmp/traces/trace.bin")
    .max_file_size(100 * 1024 * 1024)
    .max_total_size(500 * 1024 * 1024)
    .build()?;

let guard = TelemetryCore::builder().writer(writer).build()?;
guard.enable();

let mut main_builder = tokio::runtime::Builder::new_multi_thread();
main_builder.worker_threads(4).enable_all();
let (main_rt, main_handle) = guard.trace_runtime("main").build(main_builder)?;

let mut io_builder = tokio::runtime::Builder::new_multi_thread();
io_builder.worker_threads(2).enable_all();
let (io_rt, io_handle) = guard.trace_runtime("io").build(io_builder)?;
```

### `dial9-viewer` crate ([#177](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/177))

The trace viewer is its own crate now: `cargo install dial9-viewer`. It serves the interactive HTML viewer locally and can browse traces on S3.

### Custom application events ([#196](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/196), [#216](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/216), [#218](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/218))

You can emit your own events into the trace. Derive `TraceEvent`, call `record_event`. They are not currently visible in the viewer although they can be loaded from the trace via the JS parser directly. Repeated string values (HTTP methods, paths, etc.) can be interned to save space on the wire.

```rust
use dial9_trace_format::TraceEvent;
use dial9_tokio_telemetry::telemetry::{record_event, clock_monotonic_ns, TelemetryHandle};

#[derive(TraceEvent)]
struct RequestCompleted {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    status_code: u32,
    latency_us: u64,
    error_message: Option<String>,
}

record_event(
    RequestCompleted {
        timestamp_ns: clock_monotonic_ns(),
        status_code: 200,
        latency_us: 1500,
        error_message: None,
    },
    &handle,
);
```

### Trace file concatenation ([#134](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/134))

Uncompressed trace files can be concatenated (`cat trace.0.bin trace.1.bin > combined.bin`) and loaded as a single trace. The decoder resets parser state at segment boundaries via reset frames.

### Added

- Time-based rotation for `RotatingWriter`: segments rotate on wall-clock boundaries (e.g. every 60s), which gives clean S3 key paths ([#136](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/136), [#179](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/179))
- `boot_id` in default S3 key layout so segments from different process starts don't collide ([#225](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/225), [#237](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/237))
- Sampling for scheduler events: record 1-in-N context switches via `SchedEventConfig::sampling_interval` ([#233](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/233))
- Optional field type modifiers and named field decode in trace format ([#216](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/216))

### Fixed

- Segments now contain all data for their time range. Previously, thread-local buffers could drain mid-rotation, causing events from one wall-clock period to land in the wrong segment (up to 8s of timestamp overlap between adjacent files). Rotation now coordinates with the flush loop: bump epoch, drain all buffers, flush, then rotate ([#224](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/224), [#186](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/186))
- Traces now include monotonic-to-realtime clock sync frames for precise wall-clock alignment ([#210](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/210), [#214](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/214))
- `perf-self-profile` compiles on macOS ([#174](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/174))
- Background worker no longer busy-loops re-processing already gzipped segments ([#154](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/154), [#155](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/155))
- `single_file` writer uses `.active` suffix so the background worker can symbolize and gzip sealed segments ([#164](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/164))
- Empty segments are no longer sealed on finalize ([#127](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/127))
- Blocking thread TIDs are captured directly instead of guessed by name ([#120](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/120))
- Rate-limited internal tracing/logging to prevent log spam ([#209](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/209))

### Viewer

- Binary search for CPU sample attachment, faster rendering ([#201](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/201), [#143](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/143))
- Relative/absolute time toggle ([#146](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/146))
- Gzip decompression in JS parser: load `.bin.gz` files directly ([#178](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/178))
- Escape stack frame names in flamegraph tooltips ([#142](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/142))
- Handle truncated frames without crashing ([#98](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/98))

### Breaking changes

- `SamplerConfig` and `CpuProfilingConfig` now use builders instead of struct-literal construction, consistent with `SchedEventConfig` ([#244](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/244))

### Internal

- Removed recorder mutex; events encode directly into thread-local buffers ([#122](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/122), [#133](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/133), [#135](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/135))
- `graceful_shutdown` is synchronous now ([#151](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/151))
- Public API cleanup and lints ([#175](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/175), [#129](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/129))
- Bencher integration for continuous overhead tracking ([#150](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/150))

## [0.2.0](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.1.1...dial9-tokio-telemetry-v0.2.0) - 2026-03-20

0.2.0 brings two major improvements:

1. Support for publishing traces to S3
2. Migration to the new trace format (dial9-trace-format). This format is self describing, extremely compact, compressible and fast to write. This will set us up to easily add application level telemetry in the future.

For setting it up in production applications, the new `.install(true/false)` method makes it easy to have a single instantiation path for your runtime but set `install(false)` to make dial9 a complete no-op.

### Added

- Wire background symbolization into the flush/worker pipeline ([#95](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/95))
- Improve s3 writer's configuration API ([#86](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/86))
- add install() builder method for conditional telemetry install ([#85](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/85))
- Add ProcMaps frame and offline symbolizer for background symbolization ([#87](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/87))

### Fixed

- offline symbolization cleanups and optimizations ([#111](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/111))
- write segment metadata at the beginning of the file and add RotatingWriterBuilder ([#115](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/115))
- Bring back support for locations in offline symbolization ([#110](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/110))
- stop writing trailing garbage in gzip segments after graceful_shutdown ([#104](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/104))
- Fix worker spin-loop on gzip-compressed and permanently failing segments ([#102](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/102))
- _(trace_viewer)_ update format name from TOKIOTRC to D9TF in landing screen ([#103](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/103))
- _(js-decoder)_ handle truncated frames gracefully, read symbol frames even if >= MAX_EVENTS ([#98](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/98))
- clarify S3 key layout is the default, not the only option ([#89](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/89))
- add missing crates.io metadata ([#84](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/84))
- thread-local buffer not flushing on drop ([#54](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/54))

### Other

- _(trace-parser)_ consolidate per-branch cap checks into early continue ([#116](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/116))
- fix flaky worker park test ([#117](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/117))
- Harden flush path with ArrayQueue & emit metrics ([#97](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/97))
- Update demo trace to have symbols ([#105](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/105))
- add kptr_restrict guidance for kernel symbol resolution ([#99](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/99))
- Switch to new trace format ([#91](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/91))
- Prepare to migrate to dial9-trace-format ([#76](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/76))
- Add kernel tracepoint support to perf-self-profile ([#81](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/81))
- document tokio_unstable prerequisite for downstream consumers ([#90](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/90))
- Support kernel frames in callchains when include_kernel=true ([#77](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/77))
- Enable perf_event_open tests in CI ([#78](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/78))
- S3 reporter ([#60](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/60))
- Viewer UX: hint toasts, help overlay, keyboard accessibility ([#69](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/69))
- Backport analysis algorithms from trace viewer into core ([#35](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/35)) ([#63](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/63))
- Add SegmentMetadata event (wire code 11) ([#66](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/66))
- Remove SimpleBinaryWriter, use RotatingWriter everywhere ([#65](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/65))
- Track task spawn/terminate events and show active task count in trace… ([#48](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/48))
- Track blocking pool threads in CPU profiler via ThreadRole ([#52](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/52))
- Docs improvements & fix ui paper cuts ([#57](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/57))

## [0.1.1](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.1.0...dial9-tokio-telemetry-v0.1.1) - 2026-03-03

- Fix trace viewer crash when loading trace from URL parameter ([#42](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/42))
- Improve symbolization and include docs.rs links in call frames ([#39](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/39))
- Add demo trace ([#40](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/40))
- fix: take_rotated() was inside debug_assert, never ran in release builds ([#41](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/41))

## [0.1.0](https://github.com/dial9-rs/dial9-tokio-telemetry/releases/tag/dial9-tokio-telemetry-v0.1.0) - 2026-03-01

### Other

- Update readme and allow tests to pass on macOS ([#22](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/22))
- Add Cloudflare Workers configuration ([#29](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/29))
- Support Compilation on MacOS ([#16](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/16))
- Enable CPU profiling in metrics service and extract client binary ([#12](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/12))
- Integrate CPU profiling into Dial9 ([#11](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/11))
- Initial implementation of tracking task wakes ([#4](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/4))
- Convert to workspace, move crate into dial9-tokio-telemetry/ ([#5](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/5))
