# Smoke Test

> **Status:** proposed.

A service builds a tester once with `handle.smoke_tester().build()`, calls `tester.run().await` before going live, and gets a report on whether its telemetry works: recording, CPU sampling, symbolization, and S3 upload.

## The problem

Services using dial9 have no way to check, before going live, that telemetry works. These fail without any visible error today:

- CPU profiling can fail to start. `with_cpu_profiling` logs a warning and skips the source, so the trace has no CPU samples.
- CPU sampling can fall back from perf to ctimer, which samples only threads dial9 tracks.
- The pipeline may not symbolize: a custom pipeline without a symbolize stage, compression before symbolization, or a binary with no symbol table.
- S3 upload can fail on IAM, region, encryption, or network problems. It shows only when a segment uploads, minutes after start. A failed region lookup falls back to `us-east-1` with a warning.

The uploader builds its client and resolves the region when the pipeline worker starts, because `WorkerLoop::new` awaits every processor's `initialize()`. Credentials and `PutObject` stay untested until the first upload.

## Requirements

The check reports, in a form a preprod stage can fail a deploy on:

1. Whether CPU samples arrive, and which backend produces them.
2. Whether the pipeline resolves symbols.
3. Whether an upload to S3 succeeds.

All three are met once PRs 2, 3 and 4b land (see [Rollout](#rollout)).

Not checked: memory profiling, process resource sampling, Linux socket sampling, CUDA, taskdump, sched-wait sampling, and scheduler sampling (see [Follow-ups](#follow-ups)).

## Terms

- **perf**: the Linux `perf_event_open` interface. The CPU profiler's default backend. It samples the thread that starts the profiler and every thread descended from it (`inherit`).
- **ctimer**: the CPU profiler's fallback when perf is blocked. It arms a per-thread timer on the thread's CPU clock, so it samples only threads dial9 tracks.
- **Source**: a data producer the recorder drains, such as `CpuProfiler`. The **flush thread** drains every source about every 5ms, while recording is enabled.
- **Enrollment**: `Dial9Handle::track_current_thread()` on a thread. It runs every source's `on_thread_start`, which is how ctimer and the scheduler profiler start watching the thread. A Tokio worker enrolls on its first poll.
- **Attached runtime**: a Tokio runtime built through `attach_tokio_runtime` on the recorder's handle.
- **Pipeline worker**: the thread that runs the segment processors (symbolize, gzip, write-back, S3 upload).
- **Stimulus**: a short CPU burn that gives the CPU profiler something to sample on an idle service. It runs on a new thread named `dial9-smoke`, the **stimulus thread**.
- **Passive window**: how long the CPU check watches the sample counter before it runs the stimulus.

## Design principles

1. **Touch as little production code as possible.** Read state that already exists. When a change is needed, keep it off the per-event and per-segment paths, and out of the event format.
2. **No work when the check isn't running**, beyond constant bookkeeping off the per-event path (listed in [Cost](#cost)). When it runs, its cost is bounded.
3. **Smoke-test activity is labeled.** Anything the check adds to a trace can be told apart from application activity, or never reaches the trace. Exceptions are listed in [Labeling](#labeling-smoke-test-activity).
4. **Status is unambiguous.** A caller can act on a result without parsing text.
5. **Prove the running instance.** Prefer checking production's own objects over copies. This pulls against 1; [Symbolization](#symbolization) and [S3 upload](#s3-upload) are where the two meet.

## Summary

- A tester is built once and keeps its own cache and lock. See [Entry point](#entry-point).
- Each check reports one of five states; non-`Ok` states other than `Failed` carry a typed reason. See [Status](#status).
- A profiler that fails to start leaves a `StartFailed<T>` placeholder, so "failed to start" and "not configured" differ. See [CPU sampling](#cpu-sampling).
- The CPU check watches a sample counter, then proves an idle service with a 50ms burn on the `dial9-smoke` thread. See [The stimulus](#the-stimulus).
- Symbolization uses a separate symbolizer, plus the pipeline's stage order and worker state that the pipeline records. See [Symbolization](#symbolization).
- The S3 check uploads a marker with the client production resolved in `initialize()`. See [S3 upload](#s3-upload).
- Each check caches separately; failures are never cached. See [Caching and concurrency](#caching-and-concurrency).

## Where to call it

From a startup probe, a deploy gate, or a preprod stage that fails the deploy on an unhealthy report. Log the report either way.

Don't gate a readiness endpoint on it: an S3 outage would fail readiness on every instance and take the fleet out of rotation.

**Probe timeout.** Set it above 20s (`2 × check_timeout`) when the check runs in a startup probe with at most one other caller sharing the tester. Each further concurrent caller adds one `check_timeout`. Within that bound, a slow check returns its own "timed out" result instead of the probe cutting it off.

Each check is capped at `check_timeout` (10s by default), and the checks run concurrently, so one `run()` takes at most about `check_timeout`. Callers queue on the tester's lock, and a caller behind an uncacheable result runs the checks again, so a caller with `k` runs ahead of it waits up to `(k + 1) × check_timeout`. The kubelet runs each probe type one at a time per container, but different types run concurrently, and liveness and readiness start only after the startup probe succeeds.

A typical cold run is much shorter: the 200ms passive window, a 50ms burn plus up to 300ms waiting for its samples, the first symbolization (measured: 35–48ms on ~50MB binaries), and one `PutObject` (not measured).

## API

### Entry point

```rust
use dial9::Dial9HandleTokioExt;

// Once, at startup:
let tester = handle.smoke_tester().build();

// Where the check runs:
let report = tester.run().await;
if !report.is_healthy() {
    tracing::error!("dial9 smoke test failed:\n{report}");
}
```

`smoke_tester()` is a method on `Dial9HandleTokioExt`. The trait is sealed, so adding a method isn't breaking. It returns a `bon` builder whose start argument is the handle; every setting is a defaulted setter. `SmokeTester::builder` is `pub(crate)`, so `handle.smoke_tester()` is the one public way to build a tester. `dial9::smoke_test` re-exports the types.

```rust
pub struct SmokeTester {
    handle: Dial9Handle,
    settings: Settings,
    cache: tokio::sync::Mutex<Cache>,
}

impl SmokeTester {
    pub async fn run(&self) -> SmokeTestReport {
        // Held for the whole run: a concurrent caller waits, then reads the cache.
        let mut cache = self.cache.lock().await;
        // ...
    }
}
```

Build the tester once: it owns the cache that stops repeated runs from each burning CPU and uploading.

`run()` needs a Tokio runtime with the time driver enabled. The `run()` future must stay `Send`, because callers spawn it. It awaits nested joins and timeouts; unboxed, `tokio::spawn(async move { tester.run().await })` fails to compile with "queries overflow the depth limit". Boxing the inner checks fixes it, at two allocations per run, and 1a asserts it at compile time. The assertion is a closure coerced to a function pointer, so nothing is unused under clippy's `-D warnings`:

```rust
let (cpu, (symbolization, s3)) = tokio::join!(
    Box::pin(self.cpu_sampling(..)),
    Box::pin(self.pipeline(..)),
);

const _: fn(&'static SmokeTester) = |t| {
    fn assert_send<X: Send>(_: X) {}
    assert_send(t.run());
};
```

### Settings

| Setting | Default | Meaning | PR |
|---|---|---|---|
| `cache_ttl` | 1 hour | How long a passing CPU or S3 result is reused. `Duration::ZERO` re-checks every run. Symbolization ignores it (see [Caching](#caching-and-concurrency)) | 1a |
| `check_timeout` | 10s | Upper bound for each check, including waiting for the pipeline worker to start | 1a |
| `cpu_sampling` | `true` | Run the CPU check | 1c |
| `passive_window` | 200ms | How long to watch the sample counter before the stimulus | 1c |
| `stimulus` | `true` | Run the stimulus when the passive window saw no samples | 2 |
| `stimulus_cpu_time` | 50ms | CPU time the stimulus burns | 2 |
| `s3_upload` | `true` | Run the S3 check | 3 |
| `symbolization` | `true` | Run the symbolization check. Turn off for an app that symbolizes offline on purpose | 4b |

### Status

```rust
#[non_exhaustive]
pub enum CheckStatus {
    Ok,
    #[non_exhaustive]
    Degraded { reason: DegradedReason, detail: String },
    #[non_exhaustive]
    Unverified { reason: UnverifiedReason, detail: String },
    #[non_exhaustive]
    Disabled { reason: DisabledReason, detail: String },
    #[non_exhaustive]
    Failed { detail: String },
}
```

- `Ok`: working.
- `Degraded`: working with a known limit, for example the ctimer fallback.
- `Unverified`: nothing proved it works or that it's broken, and something the caller controls could prove it (turn the stimulus on, call from an attached runtime).
- `Disabled`: the check doesn't apply: not compiled in, not configured, turned off, recording paused, or the recorder isn't connected. Not a failure.
- `Failed`: should work, doesn't.

`#[non_exhaustive]` goes on each struct variant as well as the enum, so callers match `Degraded { reason, .. }` and a field can be added to a variant later. Adding the attribute after release would itself be breaking. `Disabled(X)` in this doc is shorthand for `Disabled { reason: X, .. }`.

The reason enums are `#[non_exhaustive]`. 1a ships every `DisabledReason` variant more than one check uses; the rest arrive with the PR that produces them:

| Variant | Meaning | PR |
|---|---|---|
| `DisabledReason::NotCompiled` | The check's feature isn't compiled in | 1a |
| `DisabledReason::TurnedOff` | Turned off in the settings | 1a |
| `DisabledReason::NotConfigured` | The recorder has no source or stage for it | 1a |
| `DisabledReason::Unsupported` | The platform has no profiling backend | 1a |
| `DisabledReason::NotConnected` | The handle isn't connected to a recorder | 1a |
| `DisabledReason::RecorderStopped` | The recorder has shut down | 1a |
| `DisabledReason::RecordingPaused` | Recording is paused | 1a |
| `DegradedReason::CtimerFallback` | CPU samples come from ctimer | 1c |
| `UnverifiedReason::StimulusOff` | Idle, and no stimulus runs (none exists before PR 2, or `stimulus(false)`) | 1c |
| `UnverifiedReason::StimulusUnavailable` | The stimulus was attempted and couldn't run | 2 |
| `UnverifiedReason::HostContended` | The burn fell short of its CPU time | 2 |
| `UnverifiedReason::OutsidePerfTree` | The stimulus thread may be outside perf's thread tree | 2 |
| `DisabledReason::NoS3Stage` | The pipeline has no dial9 S3 stage | 3 |
| `DisabledReason::NoPipeline` | The recorder has no pipeline worker | 4b |
| `DegradedReason::NoSymbolTable` | The binary has no symbol table | 4b |

`Failed` carries only text. Whether a failure is transient or permanent can't be classified reliably: a 403 right after a deploy can be IAM propagation delay, and a timeout on a network that drops traffic is permanent. A typed field can be added to the `#[non_exhaustive]` variant later without breaking callers (see [Follow-ups](#follow-ups)).

### Report

`SmokeTestReport` is `#[non_exhaustive]` with one accessor per check, added by the PR that adds the check: `recording()`, `cpu_sampling()`, `symbolization()`, `s3_upload()`. Also:

- `checks()`: every check with its identifier, `(Check, &CheckStatus)`. `Check` is `#[non_exhaustive]`, and `Check::as_str()` gives a stable name for logs and metrics: `"recording"`, `"cpu_sampling"`, `"symbolization"`, `"s3_upload"`. Each variant ships with the PR that adds its check; adding a variant to a `#[non_exhaustive]` enum isn't breaking.
- `checked_at(Check) -> SystemTime`: when that check's status was produced, so a gate can tell a cached result from a fresh one. Every check produces a status on every run, `Disabled` included, so there is no "didn't run" case. `SystemTime`, not `Instant`, so it can be logged and compared across processes.
- `is_healthy()`: false if any check is `Failed`.
- `is_verified()`: also false if any check is `Unverified`.
- `cpu_backend() -> Option<CpuBackend>`: `Perf` or `Ctimer` when a CPU profiler is registered. `Ok` carries no detail, so the backend has its own accessor. `CpuBackend` is a `#[non_exhaustive]` enum with `as_str()`, so callers don't compare text (principle 4); switching from a string to an enum later would be breaking.
- `Display`: one line per check using `Check::as_str()`, then the backend.

```text
recording: ok
cpu_sampling: ok
symbolization: ok
s3_upload: FAILED: HTTP 403 AccessDenied: Access Denied
cpu_backend: perf
```

Every check exists in every build. A check whose feature isn't compiled in reports `Disabled(NotCompiled)`, so callers need no `cfg`.

## The checks

The checks run concurrently. Each is bounded by `check_timeout`; a check that times out reports `Failed { detail: "timed out after 10s" }` (the setting's value, formatted with `{:?}`).

### Recording

Uses `is_connected()`, `is_stopped()` and `is_enabled()` on the handle, which exist today. Read on every run, never cached.

```rust
fn check_recording(handle: &Dial9Handle) -> CheckStatus {
    if !handle.is_connected() {
        CheckStatus::failed("handle is not connected to a recorder")
    } else if handle.is_stopped() {
        CheckStatus::failed("recorder has shut down")
    } else if !handle.is_enabled() {
        CheckStatus::disabled(DisabledReason::RecordingPaused, "recording is paused")
    } else {
        CheckStatus::Ok
    }
}
```

| Handle | `recording` | CPU check | Symbolization and S3 |
|---|---|---|---|
| Not connected to a recorder | `Failed` | `Disabled(NotConnected)` | `Disabled(NotConnected)` |
| Recorder stopped | `Failed` | `Disabled(RecorderStopped)` | `Disabled(RecorderStopped)` |
| Paused | `Disabled(RecordingPaused)` | `Disabled(RecordingPaused)` | checked |
| Live | `Ok` | checked | checked |

A disconnected handle comes from four places, and all report `recording: Failed`:

- the app calls `recorder_disabled()` because it turned telemetry off;
- the trace writer couldn't be created, and `recorder_or_disabled` or `recorder_from_env` fell back to a disabled recorder (today only an `error!` log);
- a second recorder in the process;
- `Dial9Handle::current()` on a thread with no handle installed, which is a wiring bug.

The check can't tell these apart, since they produce the same handle. **If your app turns telemetry off on purpose, skip the check.** Apps using `#[dial9::main]` or `recorder_from_env()` get a disabled handle when `DIAL9_ENABLED` is off, without calling `recorder_disabled()` themselves: skip the check when it's off. Telling the cases apart is a follow-up.

The CPU check is `Disabled` while recording isn't live, because the flush thread drains sources only while recording is enabled (`flush_once`): the sample counter stops moving, and the check would otherwise report a false `Failed`. The pipeline worker keeps running while recording is paused, so the symbolization and S3 checks still run then.

### CPU sampling

**A profiler that didn't start.** Today `with_cpu_profiling` and `with_sched_events` log a warning and skip a source that fails to start (`perf-self-profile/src/recorder_ext.rs`), so `Dial9Handle::with_source` finds nothing, the same as "never configured". PR 1b registers a no-op `StartFailed<T>` in place of `T`. It records nothing, keeps the error's `kind()` and `message()`, and writes `{source}.start_error` into segment metadata once.

```rust
pub struct StartFailed<T: 'static> {
    source_name: &'static str,
    kind: io::ErrorKind,
    message: String,
    metadata_emitted: bool,
    _source: PhantomData<fn() -> T>,
}

impl<T: 'static> Source for StartFailed<T> {
    fn flush(&mut self, _ctx: &FlushContext<'_>) {}
    fn name(&self) -> &'static str { "start_failed" }
    fn segment_metadata(&mut self, out: &mut Vec<(String, String)>) {
        if !std::mem::replace(&mut self.metadata_emitted, true) {
            out.push((format!("{}.start_error", self.source_name), self.message.clone()));
        }
    }
}
```

| Found | `cpu_sampling` |
|---|---|
| `StartFailed<CpuProfiler>` on a platform without a backend | `Disabled(Unsupported)` |
| `StartFailed<CpuProfiler>` on a platform with a backend | `Failed { detail: "CPU profiling failed to start: {error}" }` |
| Neither the source nor a placeholder | `Disabled(NotConfigured)` |

A platform has a backend when `cfg(any(target_os = "linux", all(target_os = "android", target_arch = "aarch64")))` holds, the same predicate `perf-self-profile` compiles its samplers under. The error kind alone isn't enough: on Linux, blocked perf also surfaces as `ErrorKind::Unsupported` (the per-thread sampler's `SamplingMode::Period` path, and `ENOSYS` under seccomp or gVisor).

Measured: with `CpuProfilingConfig::with_perf_backend()` in a container without `CAP_PERFMON` (`perf_event_paranoid` 3), the start fails with `EPERM` and the check reports `Failed { detail: "CPU profiling failed to start: Operation not permitted (os error 1)" }` (test: `perf_only_backend_start_error_fails`). The default `Auto` backend falls back to ctimer there instead.

**Samples arriving.** PR 1b adds two accessors. `samples_seen()` counts samples drained since start, incremented on the flush thread; the check reads it through `with_source`, which takes the lock the flush thread already uses.

```rust
impl CpuProfiler {
    pub fn samples_seen(&self) -> u64;
    pub fn effective_backend(&self) -> ActiveCpuBackend; // Perf or Ctimer, never Auto
}
```

`ActiveCpuBackend` is `#[non_exhaustive]` with `as_str()`. The report defines its own `CpuBackend`, since `perf-self-profile` is an optional dependency and the report exists in every build; 1c maps one to the other, and a variant missing from `CpuBackend` maps to `None` with a warning inside `rate_limited!`, since a periodic check would log it every run.

Sampling is random, so the counter can read zero at startup even when sampling works. The check watches it for `passive_window`, polling every 10ms. A busy service produces samples on its own, which proves the same thing a forced sample would. Only if the counter doesn't move does the check run the stimulus, then wait up to 300ms for the new samples to drain.

#### The stimulus

A new thread named `dial9-smoke`, enrolled with `Dial9Handle::track_current_thread()` for the burst. Enrolling is what lets ctimer sample it. perf samples it either way.

**CPU time.** Both samplers fire on CPU time used: ctimer through a per-thread `CLOCK_THREAD_CPUTIME_ID` timer, perf at 99Hz by default (`CpuProfilingConfig::default`). So the burn loops until the thread's own CPU clock has advanced by `stimulus_cpu_time`:

```rust
fn burn_thread_cpu(target: Duration, wall_cap: Duration) -> Duration {
    let start_wall = Instant::now();
    let start_cpu = thread_cpu_time(); // clock_gettime(CLOCK_THREAD_CPUTIME_ID)
    let mut x = 0u64;
    loop {
        for _ in 0..1024 {
            x = black_box(x.wrapping_mul(31).wrapping_add(1));
        }
        let spent = thread_cpu_time().saturating_sub(start_cpu);
        if spent >= target || start_wall.elapsed() >= wall_cap {
            return spent;
        }
    }
}
```

A fixed iteration count doesn't give a fixed CPU time. Measured: in release builds the compiler removed a loop whose result was unused (417ns for 1e8 iterations) and replaced `x += i` with its closed form (1.0µs); loops that survived ran 2.4–7x faster than in debug (see [Measurements](#measurements)).

**Wall-clock cap.** The burn gives up after 20× `stimulus_cpu_time` of wall time (1s by default), so a starved thread can't hold up the check. Chosen: stopping there means the thread got under 5% of one CPU, too little to expect samples, so the result is `Unverified(HostContended)`, not `Failed`.

**Why 50ms.** Measured: the shortest burn that produced samples in every one of 20 trials. At 99Hz, perf gave at least 3 (mean 3.9) and ctimer 5 each time; at 30ms, perf gave zero samples in 2 of 20. perf varies because in frequency mode the kernel adjusts the sampling period while it runs; ctimer fires at a fixed CPU-time interval.

**Labeled.** Each CPU sample carries its thread's name. `CpuProfiler::drain` reads it from `/proc/self/task/<tid>/comm` the first time it drains a sample from that tid, then caches it by tid. The thread is new each run and named when spawned, so its samples read `dial9-smoke` (test: `stimulus_samples_are_labeled` counts samples named `dial9-smoke`: more than zero here, zero for a burn run on a worker). Two conditions keep this true:

- The thread stays tracked and alive until the check sees the counter move, up to 300ms, with a 2s backstop. Untracking closes the thread's per-thread perf buffer without draining it (`PerfSamplerImpl::stop_tracking_current_thread`), and an exited thread's `comm` file is gone.
- PR 0 evicts a tracked thread's cached name after its last samples drain, so a later thread that reuses the tid doesn't inherit the label.

**perf's thread tree.** perf samples a thread only if it descends from the thread that started the profiler. When `run()` is on an attached runtime, the stimulus thread is spawned from a worker task, since workers descend from the thread that built the runtime. Otherwise, with perf and no samples, the check reports `Unverified(OutsidePerfTree)`, not `Failed` (test: `caller_thread_created_before_profiler` calls `run()` from a thread created before the recorder and asserts `OutsidePerfTree`).

Assumption: an attached runtime was built inside perf's tree. An attached current-thread runtime driven from a thread outside it would still report `Failed`.

```rust
let body = move || {
    let enrolled = handle.track_current_thread(); // ctimer enrolls this thread
    let spent = burn_thread_cpu(cpu_time, cpu_time * 20);
    report(spent, enrolled.as_ref().err());       // sends the result back to the check
    release_rx.recv_timeout(Duration::from_secs(2)); // stay tracked until drained
};
let spawn = move || std::thread::Builder::new().name("dial9-smoke".into()).spawn(body);
if on_attached_runtime(&handle) {
    tokio::spawn(async move { spawn() }).await?; // spawn from a worker: inside perf's tree
} else {
    spawn()?;
}
```

**Refused enrollment.** `track_current_thread()` fails on the process's limits: the scheduler profiler maps a 2MB ring per tracked thread, which counts against the locked-memory limit (CI raises `memlock` for this reason), plus the open-file limit, its 256-thread cap (`max_tracked_threads`), ctimer's `timer_create` limits, or a poisoned sources lock. Each is a limit the next worker would hit too. The thread burns anyway, since perf samples it untracked:

- with perf, samples arrive and the result stands;
- with ctimer, no samples: `Failed { detail: "could not enroll the stimulus thread: {error}" }`.

Known limitation: an untracked thread's cached name isn't evicted, so a later thread reusing that tid could carry the `dial9-smoke` label.

Measured: no application worker is blocked. A 1ms ticker on a current-thread runtime stalled at most 2.6–3.1ms, against 50ms with the burn on its worker (test: `stimulus_does_not_block_the_runtime` asserts under 40ms).

| Result | `cpu_sampling` |
|---|---|
| Counter moved, perf | `Ok` |
| Counter moved, ctimer | `Degraded(CtimerFallback)`: only dial9-tracked threads are sampled |
| No samples after the stimulus | `Failed` |
| No samples, stimulus off (or before PR 2) | `Unverified(StimulusOff)` |
| No samples, the stimulus thread couldn't be spawned | `Unverified(StimulusUnavailable)` |
| No samples, burn fell short | `Unverified(HostContended)` |
| No perf samples, stimulus thread spawned outside an attached runtime | `Unverified(OutsidePerfTree)` |
| No ctimer samples, the stimulus thread couldn't enroll | `Failed` |

No samples without a stimulus is never `Failed`.

**Gap until the scheduler follow-up.** Under ctimer, a Tokio worker whose enrollment failed also loses its CPU samples. Today the error is logged and dropped (`start_sched_sampling_if_needed`), and this check doesn't report it: the stimulus proves only the stimulus thread. `cpu_sampling` is already `Degraded` under ctimer, which points at the fallback. Per-worker coverage is in [Follow-ups](#follow-ups).

### Symbolization

The check uses its own symbolizer, plus two things the pipeline records: the stage order, and the pipeline worker's state.

```rust
// dial9_core::pipeline
pub struct PipelineStatus { stages: Vec<&'static str>, worker: WorkerState }

#[non_exhaustive]
pub enum WorkerState {
    #[non_exhaustive]
    Initializing { stage: Option<&'static str> },
    Running,
    Stopped,
}

impl Dial9Handle {
    /// Reads shared state; the worker isn't involved.
    pub fn pipeline_status(&self) -> Option<PipelineStatus>;
}
```

- The builder records the stage names in order at build.
- The worker sets `Initializing { stage }` before each `initialize()`, `Running` after the last, and a drop guard sets `Stopped` on every exit path: normal exit, initialization error, panic, drain timeout.

```rust
for processor in &mut processors {
    state.set_initializing(Some(processor.name()));
    processor.initialize().await?;
}
state.set_running();
```

Every worker is briefly `Initializing` at start, so the check waits for `Running` within `check_timeout`, then reports `Failed { detail: "pipeline worker still initializing ({stage})" }`. A stopped worker is `Failed { detail: "pipeline worker not running" }`. No pipeline is `Disabled(NoPipeline)`; the S3 check reports `Disabled(NoS3Stage)` instead, since no pipeline means no S3 stage. Test: `worker_stuck_initializing` asserts a stage whose `initialize()` never returns is named in the detail.

Then the stage order. `SymbolizeProcessor` skips gzip-compressed payloads, so the order matters:

```rust
fn shape_symbolization(stages: &[&'static str]) -> Option<CheckStatus> {
    let Some(symbolize) = stages.iter().position(|s| *s == "Symbolize") else {
        return Some(CheckStatus::failed("pipeline has no symbolize stage"));
    };
    match stages.iter().position(|s| *s == "Gzip") {
        Some(gzip) if gzip < symbolize => Some(CheckStatus::failed("segment compressed before symbolization")),
        _ => None,
    }
}
```

The names are string matches across crates, so 4b tests that the default pipeline's stage names are still `"Symbolize"` and `"Gzip"`.

Then the probe: a private function in `dial9-tokio-telemetry`, `#[inline(never)]`, with a body no other function has, resolved through `OfflineSymbolizer::symbolize()`, the type the pipeline's symbolize stage wraps:

```rust
#[inline(never)]
fn symbolization_probe(x: u64) -> u64 {
    black_box(x).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5eed_d1a1_9000
}
```

Measured: release builds merge functions with identical bodies. A probe with a common body resolved to another function's name in every release variant; with a unique body it resolved to its own name in debug, release, and fat LTO. The name is matched with `contains`, not equality. The symbolizer resolves against the live `/proc/self/maps`, as `SymbolizeProcessor` does, so the probe resolves the way a sample does. It runs on `spawn_blocking`, as production does.

The check applies when a profiler that records stacks is registered: `CpuProfiler` or `SchedProfiler` (both carry callchains), or their `StartFailed` placeholders. Otherwise `Disabled(NotConfigured)`.

| Result | `symbolization` |
|---|---|
| The probe's name | `Ok` |
| `[unknown] ...`: the binary has no symbol table | `Degraded(NoSymbolTable)`: stacks show raw addresses |
| `[symbolize-failed] ...`: blazesym couldn't read the binary | `Failed` |
| No symbol table entry for the address | `Failed` |
| Another function's name | `Failed` (offset or mapping bug) |
| No symbolize stage | `Failed { detail: "pipeline has no symbolize stage" }` |
| Compression before symbolization | `Failed { detail: "segment compressed before symbolization" }` |
| Worker stopped or stuck initializing | `Failed` |
| No pipeline worker | `Disabled(NoPipeline)` |
| No stack-recording profiler | `Disabled(NotConfigured)` |
| Platform without a backend | `Disabled(Unsupported)` |

Tests: `missing_symbolize_stage` and `compression_before_symbolization` assert both shape failures; `default_pipeline_symbolizes` asserts `Ok` for the default pipeline, run in debug, release and fat-LTO builds.

`[unknown]` appears only when the binary has no `.symtab` (`strip = "symbols"`, `strip --strip-all`). The release default (`strip = "debuginfo"`) and `strip -g` keep it, so names resolve without file and line; that is `Ok`.

Measured: the first symbolization parses the binary, 35–48ms and 15–22MB on ~50MB binaries, mostly freed when the symbolizer drops. The check drops it after each run rather than keeping a second symbolizer alive.

### S3 upload

**The client.** Each dial9 S3 stage publishes the client it resolved in `initialize()` (region-corrected, with production's credentials) into an `S3ClientSlot`, owned by `dial9-destinations-s3`, since that crate can't name `dial9-tokio-telemetry`'s types.

Every dial9 S3 builder path (`with_s3_uploader`, `with_s3_uploader_client`, `with_s3_uploader_client_future`, and a custom pipeline's `.s3()`, `.s3_with_client()`, `.s3_with_client_future()`) adds its config and slot to the recorder's one no-op `PipelineS3Targets` source, through `RecorderBuilder::source_or_insert` (PR 4a). The check uses production's own client: same credentials and region (the marker overrides timeouts, below), including a client built by `with_s3_uploader_client_future`, which only the worker can await.

This changes the production initialize path: one clone and one store per worker start.

```rust
// dial9-destinations-s3
impl S3PipelineUploader {
    pub fn client_slot(&self) -> S3ClientSlot; // filled in initialize()
}
```

Reusing production's client is also why credentials stay current: the SDK refreshes temporary credentials (instance role, ECS, IRSA, SSO, assume-role) about 10s before expiry, and re-reads static environment keys every 15 minutes (`aws-smithy-runtime` identity cache defaults).

**Several S3 stages.** `with_source` returns only the first source of a type, and `RecorderBuilder::source` adds another each call, so every S3 path adds its target to one source with `source_or_insert`. A target whose stage isn't in the running pipeline never fills its slot, and the check skips it: an uploader replaced by a later `with_s3_uploader*` call, or by a custom pipeline (tests: `replaced_s3_uploader_checks_the_live_one`, `custom_pipeline_overrides_s3_uploader`). One `s3_upload` status: `Failed` if any target fails (the detail names the bucket), `Disabled(NoS3Stage)` if none is registered, `Ok` otherwise.

**The worker gate.** The slot is filled in `initialize()`, so the check waits for `Running` the same way symbolization does, and fails the same way.

**The upload.**

- Key: `S3Config::smoke_test_key()`, `{prefix}/smoke-test/service={service}/instance={instance}.txt`. Outside the `version=1/...` segment tree, next to `dumps/`. Per instance, so it answers "did every instance pass?". `instance=` defaults to the hostname, which is the pod name on Kubernetes, so a new object appears per pod per deploy. **The prefix needs a lifecycle rule covering `smoke-test/`**, as it already needs one for per-instance segment objects, which pile up the same way. The alternative, keying by service only, gives a fixed object count but loses the per-instance answer.
- The viewer ignores the marker because of `.txt`: server-side segment discovery (`is_trace_segment` in `dial9-viewer`) accepts only `.bin`/`.bin.gz`. Keep a non-`.bin` extension if the key changes.
- Body: boot id and unix time, `text/plain`. Never deleted: production needs only `s3:PutObject` and `s3:ListBucket`, so a delete would test a permission the app doesn't use.
- **One request builder.** The segment upload, the dump manifest and the marker build their `PutObject` through one shared function, so a later SSE, ACL or checksum setting applies to all three. The builder sets the bucket, key, content type and the security settings; metadata stays per call. Today the segment and the marker set `service` and `boot-id` metadata and the manifest sets none; none sets SSE; content types differ by design.
- **Timeout.** Production's dial9-built client allows `operation_attempt_timeout` (30s) per attempt plus the SDK's retries, longer than `check_timeout` (10s). A network that drops traffic would always report a bare "timed out" with no AWS error. So the marker's `PutObject` overrides the timeout and retries on that one call. The timeout comes from the time left on the check's deadline, since waiting for the worker shares it, so the SDK gives up first and its last error reaches the report.

  The override merges with the client's `TimeoutConfig` field by field (`aws-smithy-runtime` resolves it through `MergeTimeoutConfig`): the fields it sets win, and the connect and read timeouts stay the client's. It sets the per-attempt timeout too, because the client's 30s per attempt would leave no room for a retry. It still uses production's client and credentials:

  ```rust
  let left = deadline.saturating_duration_since(Instant::now());
  if left.is_zero() { return worker_wait_timed_out(); } // report the wait, not an SDK timeout
  put_request(&client, &config, &key, "text/plain")
      .customize()
      .config_override(
          aws_sdk_s3::config::Builder::new()
              .timeout_config(TimeoutConfig::builder()
                  .operation_timeout(left * 8 / 10).operation_attempt_timeout(left * 4 / 10).build())
              .retry_config(RetryConfig::standard().with_max_attempts(2)),
      )
      .send()
      .await
  ```

  An override with no timeout fields set counts as disabled and turns every timeout off, so keep at least one field set.

- Encryption: the `PutObject` also exercises the bucket's default encryption. With SSE-KMS the role needs `kms:GenerateDataKey`.
- Error detail: `err.to_string()` prints `"service error"` for every failure, so service errors use `HTTP {status} {code}: {message}` from `ProvideErrorMetadata`, and timeout, dispatch and response errors use the error kind plus `DisplayErrorContext`, which prints the source chain:

  ```rust
  fn put_error_detail(e: &SdkError<PutObjectError>) -> String {
      let kind = match e {
          SdkError::ServiceError(ctx) => return format!(
              "HTTP {} {}: {}",
              ctx.raw().status().as_u16(),
              ctx.err().code().unwrap_or("<no error code>"),
              ctx.err().message().unwrap_or("<no error message>"),
          ),
          SdkError::TimeoutError(_) => "timeout",
          SdkError::DispatchFailure(_) => "dispatch failure",
          SdkError::ResponseError(_) => "unparseable response",
          _ => "SDK error",
      };
      format!("{kind}: {}", DisplayErrorContext(e))
  }
  ```

`S3Config::smoke_test_key()`, the marker upload, `S3ClientSlot` and `S3PipelineUploader::client_slot()` are public only because the check calls them from another crate, so they are `#[doc(hidden)]`, outside the semver commitment. The upload returns `Result<S3SmokeTestOutcome, S3SmokeTestError>`, both `#[non_exhaustive]` with private fields; the outcome ships empty, the error has `bucket()`, `key()` and `detail()` and implements `Display` and `Error`.

| Result | `s3_upload` |
|---|---|
| Upload succeeded | `Ok` |
| Upload failed | `Failed { detail }` |
| Worker stopped or stuck initializing | `Failed` |
| No dial9 S3 stage | `Disabled(NoS3Stage)` |
| Built without `worker-s3` | `Disabled(NotCompiled)` |

Tests (fake S3, all platforms): a healthy upload writes one marker at `smoke_test_key()` and nothing under `version=1/`, and a rerun overwrites it; an always-failing client gives `Failed` with the real error; a pipeline with no S3 stage uploads nothing. Real segments may upload during a test, so tests tell them apart by key.

Known limitations:

- The check doesn't see the running uploader's circuit breaker.
- Region lookup never fails: when `HeadBucket` fails, `detect_bucket_region` falls back to the `x-amz-bucket-region` header, then to `us-east-1`. A missing `s3:ListBucket` isn't reported.
- IAM policies scoped to a custom `key_fn` layout may not cover `{prefix}/smoke-test/`.
- A custom S3 stage that isn't built by dial9 registers no target and reports `Disabled(NoS3Stage)`.

## Caching and concurrency

| What | Cached | Why |
|---|---|---|
| Recording | never; it gates the other checks before the cache is read | a pause or stop shows on the next run, and a cached result never hides it |
| CPU sampling (`Ok`, `Degraded`, `Disabled`) | for `cache_ttl` | a passing result needs no new poll or stimulus |
| Worker state and stage order | never | a worker that stops after a cached pass shows on the next run |
| Symbolization (not `Failed`/`Unverified`) | for the life of the tester; `cache_ttl` doesn't apply | the binary can't change during the process |
| S3 (not `Failed`/`Unverified`) | for `cache_ttl` | without it, a check run on a schedule uploads every run |
| `Failed`, `Unverified` | never | the next run re-checks |

- CPU sampling, symbolization and S3 are cached apart. Test: `failing_symbolization_does_not_re_upload` asserts three runs with symbolization `Failed` upload one marker.
- While a check keeps failing, it runs again on every run: a failing S3 check uploads every run, and a host reporting `HostContended` burns every run.
- `run()` holds the tester's lock for the whole run. A concurrent caller waits, then reads the cache; it runs the checks again only when the first result wasn't cacheable.
- A symbolization that times out keeps running on the blocking pool. The next run waits for that task instead of starting a second symbolizer.
- Waiting for the pipeline worker counts against the same `check_timeout` as symbolization and S3: one deadline for the whole check.
- A tester is bound to one handle. After a recorder restart it reports `RecorderStopped` until rebuilt.

## Cost

| Addition | Runs on | When no check runs | Per run |
|---|---|---|---|
| `samples_seen` counter | flush thread | one increment per drained sample | a read under the sources lock |
| `StartFailed<T>` placeholder | flush thread | a no-op call per 5ms cycle and per tracked-thread start and stop, only after a start failure | a lookup |
| `PipelineS3Targets` | flush thread | a no-op call per 5ms cycle and per tracked-thread start and stop | a lookup |
| `S3ClientSlot` | pipeline worker | one clone and store per worker start | a read |
| Pipeline stage order and worker state | builder, worker start and exit | none | a read |
| Stimulus | the stimulus thread | none | 50ms of CPU and one thread spawn, only when the passive window saw nothing; once per `cache_ttl` while passing, every run while failing |
| Symbolization | blocking pool | none | the first parse, 35–48ms and 15–22MB on ~50MB binaries, freed on drop |
| S3 marker | the caller's task | none | one `PutObject`; once per `cache_ttl` while passing, every run while failing |

No event format changes; `StartFailed<T>` adds a segment metadata key.

## Labeling smoke-test activity

| Activity | Reaches the trace? | Labeled |
|---|---|---|
| Stimulus CPU samples | yes | thread name `dial9-smoke` |
| The check's symbolizer parse | yes, as CPU samples | no: its thread is named `dial9-symbolizer`, the same as production's. Production pays the same parse at the first CPU segment |
| S3 `PutObject` | yes, as a few CPU samples on the caller's worker | no |
| Scheduler samples on the stimulus thread, when `SchedProfiler` is configured | yes: enrollment starts it, and blocking in `recv_timeout` switches context | by tid only: scheduler samples carry no thread name, and the stimulus's CPU samples name that tid `dial9-smoke` |
| `run()`'s own polls, the task that spawns the stimulus thread, the symbolization `spawn_blocking` | yes, as Tokio task events | no |
| The marker object | no | `.txt` under `smoke-test/`, ignored by the viewer |
| A profiler's start error | yes, segment metadata | key `{source}.start_error`, written once, so only the first segment carries it |

A viewer can label or filter stimulus samples by thread name today, with no format change.

## Alternatives considered

| Alternative | Why not |
|---|---|
| `handle.smoke_test(settings).await`, running directly | The handle holds no cache. Without one, concurrent and repeated calls each burn and upload |
| A public `SmokeTester::builder(handle)` beside `handle.smoke_tester()` | Saves only the trait import, and takes the handle by value, so callers write `.clone()` |
| Four states (`Ok`, `Degraded`, `Disabled`, `Failed`) | An idle service with nothing proven has no state that fits: `Failed` blames it, `Degraded` hides that nothing was proven |
| Reasons as text only | Callers would parse strings (principle 4) |
| A typed failure kind (transient or permanent) in the first release | Can't be classified reliably; addable later (see [Follow-ups](#follow-ups)) |
| A reason on the disabled handle (telemetry off on purpose vs writer failure vs wiring bug) in the first release | Touches `dial9-core`'s handle, recorder and env config; "skip the check if you turned telemetry off" covers the intentional case for now |
| The caller declares what it configured, e.g. `expect_cpu_sampling(true)` | The caller mirrors its telemetry config, and the report can't say why a profiler didn't start |
| Start errors in `dial9-core` shared state | A core change for something only `perf-self-profile` produces |
| Counting samples by decoding segments | Needs the writer's files, costs a decode per run, and lags a segment rotation |
| A stimulus task on an application worker (`block_in_place`) | Unlabeled samples, a 50ms stall on a current-thread runtime, and no stimulus outside an attached runtime. It matched the dedicated thread only on an idle attached runtime |
| Not burning when the stimulus thread's enrollment is refused | Loses the perf proof |
| A public API to evict an untracked thread's name | One more public item for a rare case; recorded as a known limitation |
| A separate symbolizer and upload only | Misses a pipeline without `.symbolize()`, compression first, and a worker stuck in `initialize()` |
| A probe segment through the running worker | Would also prove the circuit breaker, but needs about 800 lines in `dial9-core`: a request slot in the worker loop, a separate run path, a stop guard, and branches in `WriteBack` and `S3Upload`; custom stages would see the probe too |
| The check builds its own S3 client from the config | A copy of production's client: rebuilds credentials every run, can't use a future-built client, and needs its own region resolution |
| An iteration-count burn | The optimizer removes or shrinks it |
| Scheduler sampling in this work | Beyond the requirements; planned in [Follow-ups](#follow-ups) |
| Checking for a debug-info section | A binary can have the section and still fail to resolve |

## Rollout

No PR targets another PR's branch. A PR that depends on another waits for it to merge, then rebases on `main`. Prerequisite: #897 merges first. It switches CI's `dial9-core`-alone test step to nextest, which the nextest step in [Test commands](#test-commands) assumes.

The code to start from is on `feat/smoke-test-api`. It implements the chosen design for all eight PRs; each PR takes its part. The integration tests cited below are in `dial9-tokio-telemetry/tests/smoke_test.rs` there.

| PR | Scope | Crates | Depends on | Size (lines with tests, estimated) |
|---|---|---|---|---|
| 0 | Evict a stopped tracked thread's cached name | perf-self-profile | none | ~40 |
| 1a | Types, builder, recording check, cache and lock (tested with a stub check), `Check`, `checked_at`, `Send` assertion, README section and an example | tokio-telemetry, dial9 | none | ~500 |
| 1b | `StartFailed<T>`, `samples_seen()`, `effective_backend()` | perf-self-profile | none | ~150 |
| 1c | CPU check, passive window only | tokio-telemetry | 1a, 1b | ~250 |
| 4a | `pipeline_status()`, `PipelineStatus`, `WorkerState`, the exit guard, `RecorderBuilder::source_or_insert` | dial9-core | none | ~350 |
| 2 | The stimulus: burn, perf-tree spawn, drain wait | tokio-telemetry | 1c, 0 | ~300 |
| 3 | S3 check: client slot, multi-target registration, worker gate, per-call timeout override, shared request builder | destinations-s3, tokio-telemetry | 1a, 4a | ~550 |
| 4b | Symbolization: probe, stage order, stack-profiler gate | tokio-telemetry | 1a, 4a | ~450 |

Order (PR numbers are names, not merge order):
1. 0, 1a, 1b and 4a, in parallel.
2. 1c, after 1a and 1b. This is the first API milestone: the report and a CPU check for a busy service. Don't gate startup on it: an idle service reports `Unverified(StimulusOff)`.
3. 2, 3 and 4b, in parallel once their dependencies land. The requirements are met when all three have.

Public items per PR, for the semver review:

| PR | New public items |
|---|---|
| 0 | none |
| 1a | `smoke_test` module: `SmokeTester`, `SmokeTesterBuilder`, `SmokeTestReport`, `CheckStatus`, `DegradedReason`, `UnverifiedReason`, `DisabledReason`, `Check` with `Check::Recording`; `Dial9HandleTokioExt::smoke_tester()`; `dial9::smoke_test` re-export |
| 1b | `StartFailed<T>` with `kind()` and `message()`; `CpuProfiler::samples_seen()`, `CpuProfiler::effective_backend()`, `ActiveCpuBackend`; the `{source}.start_error` segment metadata key |
| 1c | `SmokeTestReport::cpu_sampling()`, `cpu_backend()`, `CpuBackend`, `Check::CpuSampling`; setters `cpu_sampling`, `passive_window`; reason variants |
| 4a | `Dial9Handle::pipeline_status()`, `dial9_core::pipeline::{PipelineStatus, WorkerState}`, `RecorderBuilder::source_or_insert()` |
| 2 | setters `stimulus`, `stimulus_cpu_time`; reason variants |
| 3 | `SmokeTestReport::s3_upload()`, `Check::S3Upload`; setter `s3_upload`; reason variants. `#[doc(hidden)]`: `S3Config::smoke_test_key()`, the marker upload with `S3SmokeTestOutcome` and `S3SmokeTestError`, `S3ClientSlot`, `S3PipelineUploader::client_slot()` |
| 4b | `SmokeTestReport::symbolization()`, `Check::Symbolization`; setter `symbolization`; reason variants |

The burn and the symbolization probe are private to `dial9-tokio-telemetry`, so they add no public items.

### PR 0: evict a stopped thread's cached name

`CpuProfiler::on_thread_stop` records the tid; the next `drain` names the thread's last samples, then removes the entry. This fixes the stale-name bug for tracked threads, which makes the `dial9-smoke` label safe to filter on. Untracked threads' names are still never evicted.

Test: `stopped_thread_name_is_evicted_after_the_next_drain` asserts the name survives the stop and is gone after one drain (Linux).

### PR 1a: framework and recording check

The API decision, reviewed on its own. Useful by itself as a wiring check: a disconnected handle fails. Recording has nothing to cache, so the cache, lock and timeout helper are tested against a crate-internal stub check. Ships a README section and an example under `dial9/examples/`.

Tests: each recording state (tests: `disconnected_handle_fails_recording`, `paused_recorder_disables_dependent_checks`); a connected recorder with no sources is healthy; `Display` prints one line per check; the stub check is reused for `cache_ttl`, re-run when `Failed`, and run once for concurrent callers. Doc examples: `cargo test -p dial9 --doc --all-features`.

### PR 1b: profiler accessors and start failures

`perf-self-profile` only. Useful before the smoke test: why a profiler didn't start becomes visible in segment metadata.

Tests: a start failure registers the placeholder and writes the metadata key once; the counter advances as samples drain (Linux); a forced ctimer fallback reports `ActiveCpuBackend::Ctimer`.

### PR 1c: CPU check

`cpu_sampling()` and `cpu_backend()`, the recording-state mapping, the start-failure table with the backend-platform predicate, `Degraded(CtimerFallback)`, and the passive window.

Tests: a profiler that isn't configured, fails to start, or is unsupported (`unsupported_platform_reports_why` asserts `Disabled` off Linux); a busy runtime passes; ctimer is `Degraded` (`ctimer_backend_is_degraded`); a stopped recorder isn't reported as paused; a perf-only start error is `Failed` (`perf_only_backend_start_error_fails`, run in an unprivileged container); an idle service is `Unverified(StimulusOff)`.

### PR 4a: pipeline state

`dial9-core` only. The stage names recorded at build, the `WorkerState` updates around the existing `initialize()` loop, and a drop guard in `run_background_task_inner` that sets `Stopped` on every exit path.

The state is one atomic: the index of the stage initializing, or running, or stopped. Reading it takes no lock.

Also `RecorderBuilder::source_or_insert(make, f)`: it runs `f` on the builder's registered source of that type, adding the one `make` builds if there is none. It's the build-time twin of `Dial9Handle::with_source_or_insert` and uses the same lookup. PR 3 registers its S3 targets through it.

Tests: stage names in pipeline order; `Initializing { stage }` while a stage's `initialize()` hangs; two `source_or_insert` calls share one source (`source_or_insert_shares_one_source`); `Running` after; `Stopped` after shutdown and after an initialization error. A shuttle test for the exit guard racing shutdown, with `shuttle_test!`, driving `run_background_task_inner` directly as the existing worker shuttle tests do. `./scripts/test-shuttle.sh` tests `dial9-core` alone under `cargo test`, where the recorder guard is on, so the test must not build a recorder.

### PR 2: stimulus

The `dial9-smoke` thread, the CPU-time burn, the perf-tree spawn, the drain wait, `HostContended`, and a refused enrollment reporting `Failed` under ctimer. Settings `stimulus`, `stimulus_cpu_time`.

Tests: an idle runtime passes (`idle_service_by_stimulus`); samples carry `dial9-smoke` (`stimulus_samples_are_labeled`); an unattached runtime still passes (`called_from_unattached_runtime`); no worker stalls (`stimulus_does_not_block_the_runtime`); `stimulus(false)` is `Unverified` and re-checked next run; a caller created before the profiler is `OutsidePerfTree` (`caller_thread_created_before_profiler`).

### PR 3: S3 check

`S3ClientSlot` filled by the uploader's `initialize()`, `PipelineS3Targets` registration from every dial9 S3 builder path through `source_or_insert`, the worker gate, the shared `PutObject` builder, the per-call timeout override, the marker key and upload, `put_error_detail`, and the outcome and error types. Setting `s3_upload`.

Tests: see [S3 upload](#s3-upload), plus a future-built client passing with no caller input, and two S3 stages where one fails.

Before merging, a manual run against a real bucket, which the fake S3 can't simulate: a 403 (missing `s3:PutObject`), SSE-KMS without `kms:GenerateDataKey`, and a wrong region.

### PR 4b: symbolization

The probe, the stage-order check, the stack-profiler gate, the worker wait within one deadline, reuse of a timed-out symbolization task, and the per-tester cache. Setting `symbolization`.

Tests: see [Symbolization](#symbolization), plus a stripped binary giving `Degraded(NoSymbolTable)`, and the default stage names still matching.

### Test commands

1. `cargo fmt --check`.
2. `cargo nextest run` for the workspace; `cargo nextest run --stress-duration 20s` before merging. `build()` claims a process-wide recorder guard, and a second recorder built while it's held runs disabled. The other crates' test builds turn it off through `dial9-core/test-util`, but `dial9-core`'s own tests, run alone, keep it on, so 4a's tests need nextest's one process per test.
3. `cargo clippy --all-targets --all-features` on Linux, `--features __nonlinux_all_features` elsewhere.
4. `./scripts/test-shuttle.sh`: required for 4a, which adds a shuttle test on the worker's shutdown path; run it for 3 to catch regressions. The script doesn't run `perf-self-profile`, so 0 and 1b rely on their unit tests.
5. `cargo hack check` across features. One test under `#[cfg(not(feature = "cpu-profiling"))]` asserts `Disabled(NotCompiled)`. It builds only off Linux, with `cargo nextest run -p dial9-tokio-telemetry` alone and no features: on Linux the crate's dev-dependency on itself enables `cpu-profiling`, and in a workspace run the example crates and `dial9`'s dev-dependency on itself do. It needs a step in a macOS CI job. S3's `NotCompiled` can't be tested here: the dev-dependency enables `worker-s3` on every platform, so `cargo hack check` is its only coverage.
6. Linux-only paths (perf, ctimer, symbolization) in Docker on the same architecture as the host; an emulated one is slow and unreliable for perf. In CI these tests fail, not skip, when a backend is unavailable, so a green run means they ran. Install nextest into the bind mount once, so later runs skip the build, and keep the target directory in the mount:

   ```bash
   docker run --rm --privileged --platform linux/arm64 -v "$PWD":/work -w /work \
     -v dial9-cargo-registry:/usr/local/cargo/registry \
     -e CARGO_TARGET_DIR=/work/target/linux-docker \
     -e CARGO_INSTALL_ROOT=/work/target/linux-docker/tools rust:1-slim \
     bash -c 'export PATH=/work/target/linux-docker/tools/bin:$PATH && \
              cargo install cargo-nextest --locked && \
              cargo nextest run -p dial9-tokio-telemetry -p dial9-perf-self-profile --all-features'
   ```

   `--all-features` includes `taskdump`, which compiles only with `--cfg tokio_unstable`; the mounted repo's `.cargo/config.toml` sets it. The target directory grows past 20GB with debug, release and fat-LTO builds.

## Follow-ups

Each is additive: it adds a variant, field, setter or method to a `#[non_exhaustive]` type or a builder, so existing callers keep compiling.

- **Scheduler sampling and worker coverage.**
  - A `sched_sampling` check with `SchedProfiler::samples_seen()`, and a stimulus that forces a context switch with `n` × `std::thread::sleep(1ms)`, where `n` is the profiler's `sampling_interval`. Measured: `tokio::task::yield_now` causes no OS context switch; `std::thread::sleep(1ms)` causes one (see [Measurements](#measurements)).
  - Per-worker enrollment coverage, `worker_coverage()`, `Degraded(PartialEnrollment)`, and `Failed` for any worker whose latest enrollment failed. It also closes the ctimer gap in [CPU sampling](#cpu-sampling).
  - A runtime's workers count only while at least one of its enrollment entries is alive. Entries appear on a worker's first poll and are removed when that thread exits, so dropped runtimes leave the total with no new signal. Known limits: a current-thread driver thread that outlives its runtime keeps it counted, but its entry stays too, so the report reads 1 of 1; with `tokio_unstable`, a thread inside `block_in_place` when `shutdown_background()` runs keeps its entry until that call returns, so the report shows `PartialEnrollment` until then.
- **A reason on the disabled handle**, so telemetry turned off on purpose reports `Disabled` and a writer failure or wiring bug reports `Failed` with its cause.
- **A typed failure kind** on `Failed`, once transient and permanent failures can be told apart.
- **`SmokeTestReport::into_result() -> Result<(), SmokeTestError>`**, with the report as the error's `Display`, so a preprod stage can write `tester.run().await.into_result()?`.
- **Tester sharing**: `build()` returns the recorder's existing tester for the same settings, with `run_fresh()` so a gate can force a fresh result.
- **Region-fallback reporting**: `initialize()` records why region lookup fell back and publishes it in the client slot; the check reports `Degraded(RegionFallback)`.
- **An env-var trigger** that runs the check once from the first `attach_tokio_runtime` and logs the report.
- **The viewer shows `{source}.start_error`** when a trace has no CPU samples.

Outside this work:

- Untracking drops undrained per-thread samples: `stop_tracking_current_thread` closes a thread's perf buffer without draining it, which affects a worker's last scheduler samples at shutdown.
- Untracked threads' cached names are never evicted.
- `TokioRuntimesSource` keeps a dropped runtime's metrics and keeps emitting `RuntimeMetricsSample` events for it.

## Measurements

Linux arm64 (Docker, `--privileged`, kernel 5.15), rustc 1.97.1, 20 trials each unless noted.

**Burn loop, N = 1e8, thread CPU time**

| Loop | Debug | Release |
|---|---|---|
| `x = x*31 + i`, only the result `black_box`ed | 554–894ms | 110–123ms |
| `x += i` (closed form) | 495ms | 1.0µs |
| Result unused | 557ms | 417ns |
| `black_box` every iteration | 638ms | 262ms |
| Until thread CPU time +50ms | 50.0ms | 50.0ms |

**CPU samples per burn at 99Hz**: mean samples (trials with zero samples / total)

| Burn | perf | ctimer |
|---|---|---|
| 10ms | 0.8 (4/20) | 1.0 (0/20) |
| 30ms | 2.1 (2/20) | 3.0 (0/20) |
| 50ms | 3.9, min 3 (0/20) | 5.0 (0/20) |

**Context switches per stimulus, one-worker runtime** (for the scheduler follow-up)

| Stimulus | OS voluntary switches | Sched samples |
|---|---|---|
| `tokio::task::yield_now().await` | 0 | 0 |
| `std::thread::sleep(0)` | 0 | 0 |
| `tokio::time::sleep(1ms).await` (idle worker) | 1 | 1 |
| `std::thread::sleep(1ms)` | 1 | 1 |

**Stimulus placement** (perf backend, 3 runs each)

| Scenario | Burn on a worker | Stimulus thread | None |
|---|---|---|---|
| Idle attached runtime | proven | proven | `Unverified` |
| Samples named `dial9-smoke` | no | yes | n/a |
| `run()` from a runtime not attached to the recorder | `Unverified` | proven | n/a |
| Longest stall of a 1ms ticker, current-thread runtime | 50.05–50.09ms | 2.6–3.1ms | none |
| `run()` from a thread created before the profiler | `Unverified` (not attached) | `Unverified(OutsidePerfTree)` | n/a |

**First symbolization of one address**

| Binary | First call | Second call | Extra memory while alive | After drop |
|---|---|---|---|---|
| dev, 48.6MB | 35ms | 3.2ms | +14.5MB | +0.3MB |
| release + `debug = true`, 51MB | 48ms | 3.5ms | +21.6MB | +5.4MB |
| release, ~2MB | 3–4ms | 1–4ms | ≤ +2MB | about the same |

**Probe symbol resolution by build** (unique-body probe unless noted)

| Build | Resolves to |
|---|---|
| dev | the probe |
| release, release + `debug = true`, fat LTO | the probe; a probe with a common body resolved to another function |
| `strip = "symbols"`, `strip --strip-all` | `[unknown] 0x…` |

**Pipeline check options**, each scenario run against all three, 3 runs with the same result each (symbolization on Linux arm64; S3 on macOS and Linux)

| Scenario | Separate symbolizer and upload | Separate, plus stage order and worker state (chosen) | Probe segment through the worker |
|---|---|---|---|
| Default pipeline | `Ok` | `Ok` | `Ok` |
| Custom pipeline without `.symbolize()` | `Ok`: misses it | `Failed` | `Failed` |
| `.gzip().symbolize()` (compressed first) | `Ok`: misses it | `Failed` | `Failed` |
| Healthy S3 upload | `Ok` | `Ok` | `Ok` |
| Always-failing S3 client | `Failed` | `Failed` | `Failed` |
| Worker stuck in `initialize()` | `Ok`: misses it | `Failed`, names the stage | `Failed`, names the stage |
| Circuit breaker in backoff | not seen | not seen | `Failed` |
