# Smoke-Test API

## What this is

Services using dial9 have no way to check, before going live, that telemetry works. These can fail without any visible error today:

- CPU profiling can fail to start (for example, `perf_event_open` blocked and the ctimer fallback unavailable). The builder logs a warning and skips the source, so the trace has no CPU samples.
- CPU sampling can fall back from perf to ctimer, which only samples threads dial9 tracks.
- A worker thread can fail to enroll with the per-thread sources. It then records no scheduler samples and no ctimer samples.
- The pipeline may not symbolize: a custom pipeline without a symbolize stage, compression before symbolization, or a binary with no symbol table.
- S3 upload can fail on IAM, region, encryption, or network problems. You only find out when a segment uploads.

A smoke test checks these and returns a report the caller can act on.

## Requirements

The check reports, in a form a preprod stage can fail a deploy on:

1. Whether CPU samples arrive, and which backend produces them.
2. Whether the pipeline resolves symbols.
3. Whether an upload to S3 succeeds.

It surfaces these at startup. Today credential, region, and permission problems show up only when the first segment uploads, minutes after start, and a region lookup failure falls back to `us-east-1` with only a warning.

The uploader builds its client and resolves the region when the worker starts, since `WorkerLoop::new` awaits every processor's `initialize()` (`dial9-core/src/worker/mod.rs:396`). Credentials and `PutObject` stay untested until the first upload.

## Design principles

Every decision below is weighed against these:

1. **Touch as little production code as possible.** Prefer reading state that already exists. When a change is needed, keep it off the paths that run per event or per segment, and out of the trace format.
2. **No cost when the check isn't running.** When it runs, the cost is bounded and known in advance.
3. **Smoke-test activity is labeled.** Anything the check adds to a trace can be told apart from real activity, or never reaches the trace.
4. **Status is unambiguous.** A caller can act on a result without parsing text.

A fifth goal pulls against the first: the check should prove the running instance works, not a copy of it. Where the two conflict, the options below say so.

## Where to call it

From a startup probe, a deploy gate, or a preprod stage that fails the deploy on an unhealthy report. Log or export the report either way.

Don't gate a readiness endpoint on it. An S3 outage would fail readiness on every instance and take the whole fleet out of rotation.

A cold run can take the passive window (200ms), a 50ms burn, the first symbolization (35–48ms measured on ~50MB binaries), and one `PutObject`. Kubernetes' default probe timeout is 1s, so set the probe timeout explicitly.

## API shape

### Options

**A. Extension trait method, `handle.smoke_test(config).await`.**

- Pro: matches the existing `Dial9HandleTokioExt` pattern.
- Con: `Dial9Handle` is cheap to clone and holds no state for a cache. Caching results between calls would need a hidden global per recorder.
- Con: concurrent calls (a startup probe and a liveness probe) would each run their own burst and upload.

**B. A `SmokeTester` object built once and run many times.**

```rust
let tester = SmokeTester::new(recorder.handle().clone(), SmokeTestConfig::builder().build());
let report = tester.run().await;
```

- Pro: the tester owns its cache. Concurrent `run()` calls wait for the run in progress, then read its result:

  ```rust
  pub struct SmokeTester {
      handle: Dial9Handle,
      config: SmokeTestConfig,
      cache: tokio::sync::Mutex<Cache>,
  }

  pub async fn run(&self) -> SmokeTestReport {
      // Held for the whole run: concurrent callers wait, then hit the cache.
      let mut cache = self.cache.lock().await;
      // ...
  }
  ```
- Pro: nothing global, nothing in production code.
- Con: the caller keeps one more value around.

**Recommendation: B.** It needs no production change and makes caching and concurrency explicit.

### Configuration

All fields have defaults (`bon` builder).

| Field | Default | Meaning |
|---|---|---|
| `cpu_sampling` | `true` | Run the CPU sampling check. |
| `sched_sampling` | `true` | Run the scheduler sampling check. |
| `symbolization` | `true` | Run the symbolization check. |
| `s3_upload` | `true` | Run the S3 check. |
| `sampling_stimulus` | `true` | Burn CPU and force a context switch when the passive window saw no samples. |
| `passive_window` | 200ms | How long to watch the sample counters before the stimulus. |
| `stimulus_cpu_time` | 50ms | CPU time the stimulus burns. |
| `cache_ttl` | 1 hour | How long a passing result is reused. `Duration::ZERO` re-verifies on every run. |
| `check_timeout` | 10s | Upper bound for each check. |

## The report

Checks: `recording`, `cpu_sampling`, `sched_sampling`, `symbolization`, `s3_upload`. CPU and scheduler sampling are separate checks because they come from different sources and fail in different ways.

`report.cpu_backend()` returns `Some("perf")` or `Some("ctimer")` when a CPU profiler is registered, `None` otherwise. `Ok` carries no detail, so the backend has its own accessor.

```rust
let report = tester.run().await;
tracing::info!(backend = ?report.cpu_backend(), healthy = report.is_healthy(), "{report}");
// recording: ok
// cpu_sampling: ok
// sched_sampling: degraded: 1 of 4 workers enrolled; the rest haven't run a task yet
// symbolization: ok
// s3_upload: FAILED: HTTP 403 AccessDenied: Access Denied
// cpu_backend: perf
```

`SmokeTestReport` implements `Display`, one line per check plus the backend. Every check exists in every build: a check whose feature isn't compiled in reports `Disabled`, so callers need no `cfg`.

### Status options

**A. Four states with a detail string: `Ok`, `Degraded`, `Disabled`, `Failed`.**

- Pro: small API.
- Con: an idle service where nothing could prove sampling works has no honest state. `Failed` blames a working service; `Degraded` hides that nothing was proven.
- Con: "paused", "not compiled in", "not configured", and "turned off" differ only in text.

**B. Five states: add `Unverified`.**

- Pro: "not proven" is its own state. `is_healthy()` is false only on `Failed`; `is_verified()` is also false on `Unverified`, for gates that must not pass on an unproven check.
- Con: the reason inside each state is still only text.

**C. Five states, each non-`Ok` state carrying a typed reason plus a detail string for people.**

```rust
#[non_exhaustive]
pub enum CheckStatus {
    Ok,
    Degraded { reason: DegradedReason, detail: String },
    Unverified { reason: UnverifiedReason, detail: String },
    Disabled { reason: DisabledReason, detail: String },
    Failed { detail: String },
}

#[non_exhaustive]
pub enum DisabledReason { NotCompiled, Unsupported, NotConfigured, TurnedOff, RecordingPaused, NoPipeline, NoS3Stage }
#[non_exhaustive]
pub enum UnverifiedReason { StimulusOff, HostContended, SamplingTooSparse, OutsidePerfTree, NoS3Target, ProbeBusy }
#[non_exhaustive]
pub enum DegradedReason { CtimerFallback, PartialEnrollment, NoSymbolTable }
```

- Pro: callers branch on the reason without parsing text (principle 4). New reasons can be added without breaking callers.
- Con: three more public enums to keep stable.

**Recommendation: C.** `Failed` stays a string: failures come from many places (AWS errors, stage errors), and the action is the same.

## Recording

Uses `is_connected()`, `is_stopped()` and `is_enabled()`, which exist today. No production change.

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

| Handle | Status |
|---|---|
| Not connected to any recorder | `Failed` |
| Recorder stopped | `Failed` |
| Paused | `Disabled(RecordingPaused)` |
| Live | `Ok` |

A handle that isn't connected (`Dial9Handle::disabled()`, or `Dial9Handle::current()` on a thread without a handle) is almost always a wiring bug. Reporting it as `Disabled` would make every check `Disabled` and the report healthy.

When recording isn't `Ok`, both sampling checks report `Disabled(RecordingPaused)`. The flush loop only drains sources while recording is enabled (`dial9-core/src/flush_loop.rs:42`), so the sample counters stop moving, and a sampling check would report a false `Failed`.

## CPU and scheduler sampling

### Detecting a profiler that didn't start

`with_cpu_profiling` and `with_sched_events` log a warning and skip the source when it fails to start (`perf-self-profile/src/recorder_ext.rs`). `with_source` then finds nothing, the same as "never configured".

**A. Placeholder source.** On a start error, register a `StartFailed<T>` source in place of `T`. It records nothing and writes `{source}.start_error` into segment metadata once.

```rust
// perf-self-profile/src/recorder_ext.rs
fn with_cpu_profiling(self, config: CpuProfilingConfig) -> Self {
    match CpuProfiler::start(config) {
        Ok(source) => self.source(source) /* ... */,
        Err(e) => {
            rate_limited!(Duration::from_secs(60), {
                tracing::warn!("failed to start CPU profiler: {e}");
            });
            self.source(StartFailed::<CpuProfiler>::new(CpuProfiler::SOURCE_NAME, &e))
        }
    }
}

// The check tells "failed" from "not configured":
match handle.with_source(|f: &mut StartFailed<CpuProfiler>| (f.kind(), f.message().to_owned())) {
    Some((io::ErrorKind::Unsupported, msg)) => CheckStatus::disabled(DisabledReason::Unsupported, msg),
    Some((_, msg)) => CheckStatus::failed(format!("CPU profiling failed to start: {msg}")),
    None => CheckStatus::disabled(DisabledReason::NotConfigured, "not configured"),
}
```

- Pro: the only change is the error branch in `recorder_ext.rs`.
- Pro: the start error also lands in the trace's segment metadata, so a viewer can show why CPU samples are missing.
- Con: a no-op source in the flush loop: one virtual call every 5ms cycle.
- Con: a placeholder that isn't really a source.

**B. The caller declares what it configured**, e.g. `SmokeTestConfig::expect_cpu_sampling(true)`. A missing source is then `Failed`.

- Pro: no production change.
- Con: the caller has to mirror its telemetry config in the smoke test config.
- Con: the report can't say why the profiler didn't start.

**C. Record start errors in shared state** (a list of `(source name, error)` on the recorder).

- Pro: covers any source, with no placeholder.
- Con: a change in `dial9-core` for something only `perf-self-profile` produces today.

**Recommendation: A.** Smallest change that gives the reason. B is the fallback if no production change is acceptable.

| Found | Status |
|---|---|
| Placeholder with `ErrorKind::Unsupported` | `Disabled(Unsupported)` |
| Placeholder with any other error | `Failed { detail: "... failed to start: {error}" }` |
| Neither source nor placeholder | `Disabled(NotConfigured)` |

### Seeing samples arrive

`effective_backend` is a private field on `CpuProfiler`, visible only in segment metadata. `drain()`/`for_each_sample` are `pub(crate)` and consume samples as they read them. There is no counter to read.

**A. A `samples_seen()` counter on `CpuProfiler` and `SchedProfiler`, plus `CpuProfiler::effective_backend()`.** The counter is a `u64` incremented per drained sample.

```rust
// perf-self-profile/src/cpu_source.rs, in CpuProfiler::drain (flush thread)
self.sampler.for_each_sample(|sample| {
    if sample.pid != pid {
        return;
    }
    self.samples_seen += 1;
    // ...
});

pub fn samples_seen(&self) -> u64 { self.samples_seen }
pub fn effective_backend(&self) -> &'static str { self.effective_backend }

// The check:
let seen = handle.with_source(|p: &mut CpuProfiler| p.samples_seen());
```

- Pro: the increment runs on the flush thread, never on application threads. Readers go through `with_source`, which takes the lock the flush thread already uses.
- Pro: no trace format change.
- Con: new public accessors on two sources.

**B. Read the trace.** Decode recent segments and count CPU samples.

- Pro: no production change.
- Con: needs access to segments the writer owns, costs a decode per run, and lags by a segment rotation.

**Recommendation: A.** Two accessors and one increment.

Sampling is random, so a counter can read zero at startup even when sampling works. The check watches the counters for `passive_window`. A busy service produces samples on its own, which proves the same thing a forced sample would. Only if a counter doesn't move does the check run a stimulus.

### The stimulus

It has to produce CPU time and at least one OS context switch on a thread dial9 samples.

**CPU time.** Burn until the thread's own CPU clock (`CLOCK_THREAD_CPUTIME_ID`) advances by `stimulus_cpu_time`.

```rust
pub fn burn_thread_cpu(target: Duration, wall_cap: Duration) -> Duration {
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

 An iteration-count loop doesn't work: release builds remove loops whose result is unused or has a closed form, and loops that survive run about 5x faster than in debug. A loop bounded by thread CPU time runs exactly the target in both (see [Measurements](#measurements)). Cap it at 20x the target in wall time (1s by default). If the cap stops it short, the host is too contended: `Unverified(HostContended)`.

At 99Hz, 50ms of CPU gave at least 3 perf samples and exactly 5 ctimer samples in 20 of 20 runs. 30ms gave zero perf samples in 2 of 20 runs.

**Context switch.** `std::thread::sleep(1ms)` blocks the thread, which the kernel records as one voluntary context switch. `tokio::task::yield_now` causes none, and `tokio::time::sleep` only does when the worker is idle and parks. With `sampling_interval(n)` the scheduler profiler records every `n`th switch on a thread, so the stimulus sleeps `n` times: `n` consecutive switches always include one recorded switch. Above 100 it doesn't try (over 100ms of sleeping), and an unmoved scheduler counter is `Unverified(SamplingTooSparse)`. This needs a `SchedProfiler::sampling_interval()` accessor.

```rust
let switches = match handle.with_source(|p: &mut SchedProfiler| p.sampling_interval()) {
    Some(n) if n > 100 => 0, // Unverified(SamplingTooSparse) if nothing moves
    Some(n) => n,
    None => 0,
};
for _ in 0..switches {
    std::thread::sleep(Duration::from_millis(1)); // one voluntary context switch
}
```

**Where the stimulus runs:**

**A. A task on an application worker**, inside `tokio::task::block_in_place`.

- Pro: proves a worker thread is sampled.
- Con: the samples land on an application worker's thread and look like real application activity (principle 3).
- Con: blocks one worker's OS thread for about 51ms. `block_in_place` moves the worker's queue to another thread, but that thread then enrolls again, which the enrollment count has to handle.
- Con: only works when `run()` is called from a runtime attached to the recorder. From a separate health-server runtime the stimulus runs on an untracked thread, and an idle service can only be `Unverified`.

**B. A dedicated thread named `dial9-smoke`**, enrolled with `Dial9Handle::track_current_thread()` for the duration of the burst.

```rust
let (done_tx, done_rx) = oneshot::channel();
let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
let body = move || {
    let _guard = handle.track_current_thread()?; // ctimer + sched enroll this thread
    let spent = burn_thread_cpu(cpu_time, cpu_time * 20);
    for _ in 0..switches {
        std::thread::sleep(Duration::from_millis(1));
    }
    done_tx.send(spent);
    // Stay tracked until the flush thread drains the samples.
    release_rx.recv_timeout(Duration::from_secs(2));
};
let spawn = move || std::thread::Builder::new().name("dial9-smoke".into()).spawn(body);
// Spawn from a worker when possible: perf samples only the profiler's thread tree.
if on_attached_runtime(&handle) {
    tokio::spawn(async move { spawn() }).await?;
} else {
    spawn();
}
let spent = done_rx.await?;
wait_for_samples(/* ... */).await;
drop(release_tx); // the thread untracks and exits
```

- Pro: labeled. CPU samples carry the thread name, read when a thread's first sample is drained, and this thread is new each run, so every stimulus sample shows `dial9-smoke`.
- Pro: no application worker is blocked, and it works from any runtime or none.
- Pro: `track_current_thread` enrolls the thread with every per-thread source, the same path non-Tokio threads use today, so ctimer and scheduler sampling both see it.
- Con: proves the sampling mechanism, not a Tokio worker. Worker coverage comes from [Worker enrollment](#worker-enrollment) instead.
- Con: one thread spawn per stimulus run, at most once per `cache_ttl`.
- Issue: with perf, a thread is sampled only if it descends from the thread that started the profiler (perf `inherit`). Spawned from a caller thread created before the profiler started, the smoke thread gets no perf samples, and the check would report `Failed` although the runtime's workers are sampled. Mitigation: when `run()` is on an attached runtime, spawn the smoke thread from a worker task (workers descend from the thread that built the runtime). Otherwise, with perf and no samples, report `Unverified(OutsidePerfTree)`, not `Failed`.
- Issue: the thread must stay tracked and alive until the flush thread drains its samples. Untracking closes the thread's per-thread perf buffer without draining it (`perf-self-profile/src/sys/linux/perf_sampler.rs:306`), and the thread name is read from `/proc/self/task/<tid>/comm` at drain time, so an exited thread's samples lose the name. The check releases the thread after it sees the counters move, or after a 2s cap.

**C. No stimulus.** Only the passive window.

- Pro: zero added activity.
- Con: an idle service is always `Unverified`.

Scenario results (Linux arm64, perf backend, 3 runs each):

| Scenario | A: worker | B: dedicated thread | C: none |
|---|---|---|---|
| Idle attached runtime | proven | proven | `Unverified` |
| Stimulus samples named `dial9-smoke` | no (on a worker thread) | yes | n/a |
| `run()` from a runtime not attached to the recorder | `Unverified` | proven | n/a |
| Longest stall of a 1ms ticker on a current-thread runtime | 50.05–50.09ms | 2.6–3.1ms | none |
| `run()` from a thread created before the profiler | `Unverified` (not attached) | `Failed` without the mitigation, `Unverified(OutsidePerfTree)` with it | n/a |

**Recommendation: B**, with both mitigations. It keeps application workers untouched and labels the activity. `stimulus(Off)` gives C.

### CPU sampling status

| Result | Status |
|---|---|
| Counter moved, perf | `Ok` |
| Counter moved, ctimer | `Degraded(CtimerFallback)`: only dial9-tracked threads are sampled |
| No samples after the stimulus | `Failed` |
| No samples, stimulus off | `Unverified(StimulusOff)` |
| No samples, burn fell short | `Unverified(HostContended)` |
| No perf samples, stimulus thread spawned outside an attached runtime | `Unverified(OutsidePerfTree)` |

No samples without a stimulus is never `Failed`.

### Worker enrollment

Each Tokio worker enrolls with the per-thread sources on its first poll. Enrollment can fail per worker; today the error is logged and dropped (`start_sched_sampling_if_needed` in `dial9-tokio-telemetry/src/telemetry/recorder/runtime_context.rs`). `Dial9Handle::track_current_thread` undoes every source on a thread when one fails, so a scheduler enrollment failure also removes that worker's ctimer CPU sampling.

**A. Spawn one stimulus task per worker.**

- Pro: no production change.
- Con: Tokio can't target a worker, so it gives no guarantee, and it blocks several workers.

**B. Record the latest enrollment result per worker id** in each `RuntimeContext`. Compare against `RuntimeMetrics::num_workers()`.

```rust
// dial9-tokio-telemetry/src/telemetry/recorder/runtime_context.rs
fn start_sched_sampling_if_needed(ctx: &RuntimeContext, global_id: u64) {
    if TRACKING_ATTEMPTED.with(|attempted| attempted.replace(true)) {
        return;
    }
    let result = match ctx.recorder_handle.track_current_thread() {
        Ok(guard) => {
            THREAD_TRACKING.with(|cell| *cell.borrow_mut() = Some(guard));
            Ok(())
        }
        Err(e) => {
            tracing::warn!("failed to profile worker thread: {e}");
            Err(e.to_string())
        }
    };
    ctx.enrollment.by_worker.lock().unwrap().insert(global_id, result);
}
```

- Pro: exact.
- Pro: one map insert per worker thread lifetime, in a path that already runs once per thread.
- Con: a small change in the enrollment path.
- Keyed by worker id, not thread: after `block_in_place`, another thread takes over the worker and enrolls again. The latest result for the id wins, since that thread is the one running the worker now.

**C. Leave it logged only.**

- Con: the check can't see it.

**Recommendation: B.**

| Result | `sched_sampling` |
|---|---|
| Any worker id's latest enrollment failed | `Failed { detail: "{failed} of {workers} workers failed to enroll: {error}" }` |
| Counter moved, every worker enrolled | `Ok` |
| Counter moved, some workers not enrolled yet | `Degraded(PartialEnrollment)`: those workers haven't run a task yet |
| No samples after the stimulus | `Failed` |
| No samples, stimulus couldn't run | `Unverified(...)` |

An idle multi-worker runtime reports `PartialEnrollment` at startup until every worker has run a task. That is `Degraded`, not `Unverified`, so `is_verified()` doesn't fail every startup probe on an idle service.

## Symbolization and S3

The main decision is how close to the running pipeline the check gets.

### Option 1: separate instances

The check builds its own `OfflineSymbolizer` and resolves a probe address. For S3 it calls a new `S3Config::smoke_test(client)` that resolves the region through the same `build_uploader` path production uses, then writes a marker object. The caller passes a copy of the config and client (`S3Config` is `Clone`, and the client is cheap to clone).

```rust
let tester = SmokeTester::new(
    recorder.handle().clone(),
    SmokeTestConfig::builder()
        .s3_target(S3SmokeTestTarget::new(s3_config.clone(), s3_client.clone()))
        .build(),
);

// dial9-destinations-s3
impl S3Config {
    pub async fn smoke_test(&self, client: aws_sdk_s3::Client) -> Result<(), String> {
        let timeout = self.operation_attempt_timeout;
        let key = self.smoke_test_key();
        let run = async {
            let (uploader, _) = S3PipelineUploader::build_uploader(self.clone(), client).await;
            uploader.put_smoke_marker(&key).await
        };
        match tokio::time::timeout(timeout, run).await {
            Ok(result) => result,
            Err(_) => Err(format!("timed out after {timeout:?}")),
        }
    }
}
```

- Pro: no change to the worker or to the upload path. The S3 code is new code next to the uploader, not a change to it.
- Pro: proves the binary can be symbolized, and that the config, credentials, region, `s3:PutObject`, and default bucket encryption work.
- Con: doesn't prove the pipeline symbolizes. A custom pipeline without `.symbolize()`, or compression before symbolization, passes.
- Con: doesn't see the running worker: never started, stuck in `initialize()`, or wedged.
- Con: doesn't see the running uploader: its circuit breaker, or the client built by `with_s3_uploader_client_future`, which the caller can't copy.
- Con: a second symbolizer while the check runs (15–22MB measured on ~50MB binaries, mostly freed on drop).
- Con: the S3 check needs caller input.

### Option 2: probe segment through the live pipeline

The check asks the running pipeline worker to process one synthetic, memory-only segment. It holds one `StackFrames` event with the probe address and is marked `smoke_test`. The worker replies with each stage's result and the name the `Symbolize` stage gave the address.

- Pro: proves the running pipeline symbolizes, in the right order, with the symbolizer it already has (no second copy).
- Pro: proves the running uploader, with its own client and circuit breaker, can upload.
- Pro: proves the worker is alive, and names the stage it's stuck initializing.
- Pro: the S3 check needs no caller input.
- Con: the most production change: a request slot and wake-up in `WorkerLoop`, a separate `run_probe`, a stop guard, a pass-through in `WriteBack`, and a marker branch in the S3 stage.
- Con: custom stages see the probe segment too. They need `SegmentData::is_smoke_test()` to skip side effects.
- Con: the request waits for the segment the worker is processing.

### Option 3: separate instances plus pipeline shape and worker state

Option 1, plus two things the builder and worker record once:

- The pipeline's stage names in order, written by the builder at build.
- The worker's state (`Initializing(stage)`, `Running`, `Stopped`), written by the worker at start, after each `initialize()`, and on exit.

```rust
// dial9-core
pub struct PipelineStatus { stages: Vec<&'static str>, worker: WorkerState }

#[non_exhaustive]
pub enum WorkerState { Initializing(Option<&'static str>), Running, Stopped }

impl Dial9Handle {
    /// Reads shared state only; the worker isn't involved.
    pub fn pipeline_status(&self) -> Option<PipelineStatus>;
}

// Builder, once at build:
let stages = processors.iter().map(|p| p.name()).collect();

// Worker, around its existing initialize loop:
for processor in &mut processors {
    state.set_initializing(Some(processor.name()));
    processor.initialize().await?;
}
state.set_running();
// and a drop guard sets Stopped on every exit path.
```

The check waits for `Running`, then reads the stage order:

```rust
fn shape_symbolization(stages: &[&'static str]) -> Option<CheckStatus> {
    let Some(symbolize) = stages.iter().position(|s| *s == "Symbolize") else {
        return Some(CheckStatus::failed("pipeline has no symbolize stage"));
    };
    match stages.iter().position(|s| *s == "Gzip") {
        Some(gzip) if gzip < symbolize => {
            Some(CheckStatus::failed("segment compressed before symbolization"))
        }
        _ => None,
    }
}
```

- Pro: detects a missing or misordered symbolize stage, and a worker that never started, is stuck initializing, or exited.
- Pro: no per-segment code, no probe segment, so custom stages are untouched.
- Con: still a second symbolizer and a separate client, so it doesn't see the circuit breaker or a future-built client, and doesn't prove the running uploader's own upload.
- Con: still needs caller input for S3. When the pipeline has an S3 stage but the caller passed no target, the check is `Unverified(NoS3Target)`, not silently `Disabled`.
- Issue: every worker initializes briefly at start, so `Initializing` right after build is normal. The check waits for `Running` up to `check_timeout`, and only then reports `Failed { detail: "pipeline worker still initializing ({stage})" }`.

### Comparison

| | Option 1 | Option 2 | Option 3 |
|---|---|---|---|
| Worker code changed | none | request handling, `run_probe`, stop guard | state writes at start, init, exit |
| Stages changed | none | `WriteBack`, `S3Upload` | none |
| Cost when not running | none | one atomic check before each segment | none |
| Cost per run | second symbolizer parse and memory, one `PutObject` | first parse moves to startup (production pays it anyway), one `PutObject` | same as option 1 |
| Activity in the trace | none | none: the probe never reaches the ring or the trace | none |
| Proves pipeline symbolizes | no | yes | stage present and in order |
| Proves running uploader | no | yes | no |
| Sees worker state | no | yes | yes |
| Caller input for S3 | yes | no | yes |

### Scenario results

Each scenario run against all three options (Linux arm64 for symbolization, macOS and Linux for S3):

| Scenario | Option 1 | Option 3 | Option 2 |
|---|---|---|---|
| Default pipeline, symbolization | `Ok` | `Ok` | `Ok` |
| Custom pipeline without `.symbolize()` | `Ok`: misses it | `Failed`: no symbolize stage | `Failed`: no symbolize stage |
| `.gzip().symbolize()` (compressed first) | `Ok`: misses it | `Failed`: compressed before symbolization | `Failed`: compressed before symbolization |
| Healthy S3 upload | `Ok` | `Ok` | `Ok` |
| Always-failing S3 client | `Failed` | `Failed` | `Failed` |
| Worker stuck in `initialize()` | `Ok`: misses it | `Failed`: still initializing, names the stage | `Failed`: still initializing, names the stage |
| No S3 stage, caller passes a target | `Ok` for an upload the pipeline never makes | `Disabled`: no S3 stage | `Disabled`: no S3 stage |
| `with_s3_uploader_client_future`, no target | `Disabled`: silent | `Unverified(NoS3Target)` | `Ok` |
| Circuit breaker in backoff | not seen | not seen | `Failed { detail: "in backoff" }` (can't be set from a test) |

A failing upload against a server returning 5xx takes about 6s per run, because the SDK retries. `check_timeout` (10s) covers it.

**Recommendation: option 3 for the first version.** It checks symbolization against the pipeline's actual shape, catches a worker that never came up, and adds no per-segment code. Option 2 is the step up if proving the running uploader (circuit breaker, future-built client) is required.

### The probe address (all options)

The probe is `perf-self-profile::smoke::symbolization_probe`, `#[inline(never)]`, with a body no other function has.

```rust
#[inline(never)]
pub fn symbolization_probe(x: u64) -> u64 {
    black_box(x).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5eed_d1a1_9000
}

let addr = symbolization_probe as *const () as u64;
```

 Release builds merge functions with identical bodies: a probe with a common body resolved to another function's name in every release variant measured. With a unique body it resolved to its own name in debug, release, and fat LTO. Match the name with `contains`, not equality.

`SymbolizeProcessor` resolves addresses against the live `/proc/self/maps` (`symbolize_processor.rs:68`), so a segment holding one `StackFrames` event resolves the way a real one does.

The check only runs when a `CpuProfiler` is registered: only CPU samples carry stacks. Otherwise `Disabled(NotConfigured)`.

### Symbolization status

Placeholders are matched by prefix; both end with the address, e.g. `"[unknown] 0x7f2a1c003420"`.

| Result | Status |
|---|---|
| The probe's name | `Ok` |
| `[unknown] ...`: the lookup ran but the binary has no symbol table | `Degraded(NoSymbolTable)`: stacks show raw addresses |
| `[symbolize-failed] ...`: blazesym failed, e.g. couldn't read the ELF | `Failed` |
| Another function's name | `Failed` (offset or mapping bug) |
| No symbolize stage (options 2, 3) | `Failed { detail: "pipeline has no symbolize stage" }` |
| Compression before symbolization (options 2, 3) | `Failed { detail: "segment compressed before symbolization" }` |

```rust
fn name_status(name: &str) -> CheckStatus {
    if name.starts_with("[unknown]") {
        CheckStatus::degraded(DegradedReason::NoSymbolTable, "binary has no symbol table")
    } else if name.starts_with("[symbolize-failed]") {
        CheckStatus::failed(name)
    } else if name.contains("symbolization_probe") {
        CheckStatus::Ok
    } else {
        CheckStatus::failed(format!("probe resolved to the wrong symbol: {name}"))
    }
}
```

`[unknown]` only appears when the binary has no `.symtab` (`strip = "symbols"`, `strip --strip-all`). The release default (`strip = "debuginfo"`) and `strip -g` keep it, so names resolve without file and line. That is `Ok`.

`SymbolizeProcessor` skips gzip-compressed payloads (`symbolize_processor.rs:55`), which is why stage order matters. An app that symbolizes offline on purpose turns the check off with `symbolization(false)`.

### The S3 marker (all options)

- Key: `{prefix}/smoke-test/service={service}/instance={instance}.txt`. Fixed, so each run overwrites one object per instance and the bucket needs no lifecycle rule. It sits outside the `version=1/...` segment tree, next to `dumps/`.
- Extension: the viewer ignores the marker because of `.txt`, not because of the path: segment discovery only accepts `.bin`/`.bin.gz` (`dial9-viewer/src/ingest/aggregate.rs:207`, `dial9-viewer/ui/src/lib/trace/keys.ts:51`). Keep a non-`.bin` extension if the key changes.
- Body: boot id and unix time, `text/plain`.
- Never deleted. Production needs only `s3:PutObject` and `s3:ListBucket`, so a delete would test a permission the app doesn't use.
- Timeout: wrap the whole `PutObject` in `tokio::time::timeout(operation_attempt_timeout)` (30s default). dial9 applies `operation_attempt_timeout` only to clients it builds (`s3.rs:141`), and it bounds one attempt, not the SDK's retries.
- Encryption: the `PutObject` also exercises the bucket's default encryption. With SSE-KMS the role needs `kms:GenerateDataKey`.
- Error detail: `err.to_string()` prints `"service error"` for every failure. Use `HTTP {status} {code}: {message}` from `ProvideErrorMetadata` for service errors, and the error kind plus the source chain for timeout, dispatch, and response errors.

```rust
pub fn smoke_test_key(&self) -> String {
    let suffix = format!(
        "smoke-test/service={}/instance={}.txt",
        hive_escape(&self.service_name),
        hive_escape(self.instance_path.as_str()),
    );
    match &self.prefix {
        Some(p) => format!("{p}/{suffix}"),
        None => suffix,
    }
}

fn put_error_detail(e: &SdkError<PutObjectError>) -> String {
    use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata};
    let kind = match e {
        SdkError::ServiceError(ctx) => {
            return format!(
                "HTTP {} {}: {}",
                ctx.raw().status().as_u16(),
                ctx.err().code().unwrap_or("<no error code>"),
                ctx.err().message().unwrap_or("<no error message>"),
            );
        }
        SdkError::TimeoutError(_) => "timeout",
        SdkError::DispatchFailure(_) => "dispatch failure",
        SdkError::ResponseError(_) => "unparseable response",
        _ => "SDK error",
    };
    format!("{kind}: {}", DisplayErrorContext(e))
}
```

| Result | Status |
|---|---|
| Upload succeeded | `Ok` |
| Upload failed | `Failed { detail }` |
| No S3 destination, or built without `worker-s3` | `Disabled(NoS3Stage)` |
| Circuit breaker in backoff (option 2) | `Failed { detail: "in backoff" }` |

Known limitations:

- Region lookup never fails. When `HeadBucket` fails, production falls back to the `x-amz-bucket-region` header, then to `us-east-1` (`detect_bucket_region` in `s3.rs`). A missing `s3:ListBucket` isn't detected, and a fallback can't be reported as `Degraded` unless `build_uploader` returns whether it fell back.
- IAM policies scoped to a custom `key_fn` layout may not cover `{prefix}/smoke-test/`.

### Option 2 details

**Request slot, not a channel.** `ProbeShared` holds at most one pending request, a `Notify`, a count of callers still waiting, the stage being initialized, and a stopped flag. `probe_pipeline` returns `Busy` only while another caller is still waiting. A caller that timed out leaves its request in the slot; the next caller replaces it, and the worker skips requests whose reply channel is closed.

```rust
pub(crate) struct ProbeShared {
    pending: Mutex<Option<PipelineProbeRequest>>, // at most one; replaced if its caller gave up
    notify: Notify,
    waiting: AtomicUsize,                         // callers waiting for a reply
    initializing: Mutex<Option<&'static str>>,
    stopped: AtomicBool,
}

async fn probe(&self, probe_addr: u64, timeout: Duration, upload: bool)
    -> Result<PipelineProbeReport, PipelineProbeError>
{
    if self.waiting.fetch_add(1, AcqRel) > 0 {
        self.waiting.fetch_sub(1, AcqRel);
        return Err(PipelineProbeError::Busy);
    }
    let _waiting = WaitingGuard(&self.waiting);
    let (reply, rx) = oneshot::channel();
    *self.pending.lock().unwrap() = Some(PipelineProbeRequest { probe_addr, upload, reply });
    self.notify.notify_one();
    match time::timeout(timeout, rx).await {
        Ok(Ok(report)) => Ok(report),
        Ok(Err(_)) => Err(PipelineProbeError::WorkerStopped),
        Err(_) => Err(PipelineProbeError::TimedOut { initializing: *self.initializing.lock().unwrap() }),
    }
}
```

A one-request channel doesn't work. A request left by a timed-out caller fills it until the worker reads it. While the worker is stuck, in `initialize()` or on a long upload, every later run gets `Busy`, reported as `Unverified`, and the report turns healthy. With the slot, every run times out, and the check stays `Failed`.

**Worker changes:**

- The worker waits on the `Notify` in the `select!` of `run_continuous` and `run_triggered`.
- `process_segments` checks the slot before each segment, so a probe waits for at most one segment. Check an atomic flag before taking the lock, so a worker with no pending probe pays one atomic load per segment.
- The probe runs in its own `run_probe`, not `process_segments`. The failure paths in `process_segments` call `fs.remove_sealed` and `fs.release_for_retry`, which must never see a segment that isn't in the ring. `run_probe` stops at the first failing stage, doesn't retry, and emits no metrics, so probes don't show as uploaded segments on dashboards.
- `WorkerLoop::new` awaits every processor's `initialize()` before the loop, so no probe is answered until then. The worker records the stage it's initializing; a timed-out caller reads it into `TimedOut { initializing: Some(stage) }`.
- A drop guard in `run_background_task_inner` marks the probe stopped and drops the pending request on every exit path: normal exit, initialization error, panic, drain timeout. Callers get `WorkerStopped`, not a timeout.
- The recorder installs the slot only when a worker thread actually spawned. Without one, `probe_pipeline` returns `NoPipeline`.

**Probe segment.** `SegmentRef::Memory`, index `u32::MAX`, no accounting, metadata `smoke_test=1`, `epoch_secs`, and `smoke_test.skip_upload=1` when the S3 check is off. After each stage, while the payload isn't gzip, the worker decodes it and looks up the `SymbolTableEntry` for the probe address at `inline_depth` 0, reading fields by name.

**Stages.** `WriteBack` passes the probe through (it has no file). `S3Upload` fails with `"in backoff"` when `!should_attempt()` (side-effect free, and true for an open breaker past its backoff, `connection.rs:33`); otherwise it writes the marker. The result never feeds the breaker. `Gzip` and `Symbolize` run unchanged. Custom stages see `SegmentData::is_smoke_test()` and should return probe segments unchanged; they only see one when the app runs the smoke test, so the change is opt-in.

**Cancel safety.** The probe doesn't put a timeout around `ensure_initialized`. A change that does relies on two invariants: `ensure_initialized` changes state only after each `await` completes, and a cancelled client future resumes on the next attempt (`S3PipelineUploader::with_client_future`, `s3.rs:560-565`).

**API** (`dial9-core`, behind `#[cfg(feature = "pipeline")]`, like `Dial9Handle::dump_trigger`):

```rust
impl Dial9Handle {
    // handle.probe_pipeline().probe_addr(addr).timeout(t).upload(true).send().await
    pub async fn probe_pipeline(&self, probe_addr: u64, timeout: Duration, upload: bool)
        -> Result<PipelineProbeReport, PipelineProbeError>;
}

#[non_exhaustive]
pub enum PipelineProbeError { NotConnected, NoPipeline, WorkerStopped, Busy, TimedOut { initializing: Option<&'static str> } }
```

The request carries the probe address because `dial9-core` doesn't depend on `perf-self-profile`.

| Probe error | `symbolization` and `s3_upload` |
|---|---|
| `WorkerStopped` | `Failed { detail: "pipeline worker not running" }` |
| `TimedOut { initializing: Some(stage) }` | `Failed { detail: "pipeline worker still initializing ({stage})" }` |
| `TimedOut { initializing: None }` | `Failed { detail: "timed out after {check_timeout}" }` |
| `Busy` | `Unverified(ProbeBusy)` |
| `NoPipeline` | `Disabled(NoPipeline)` |

## Caching and concurrency

- A passing sampling result (`Ok`, `Degraded`, `Disabled`) is reused for `cache_ttl`: no poll, no stimulus. `Failed` and `Unverified` are never cached, so the next run re-checks.
- Enrollment results are read on every run, not cached, so a worker that fails after a cached result shows up on the next run.
- A symbolization result never goes stale: the binary can't change during the process. Cache the result and drop the symbolizer, rather than keeping a second symbolizer alive (option 1 and 3).
- A passing S3 result is reused for `cache_ttl`. Without it, a check run on a schedule uploads on every run.
- `run()` holds the tester's lock for the whole run. Concurrent callers wait, then read the cache, so they never start a second stimulus or upload.
- Checks run concurrently, each bounded by `check_timeout`.

## Cost

| Addition | Where it runs | Cost when the check isn't running | Cost per run |
|---|---|---|---|
| `samples_seen` counters | flush thread | one increment per drained sample | a read under the sources lock |
| `StartFailed<T>` placeholder | flush thread | one no-op call per 5ms cycle, only after a start failure | a lookup |
| Enrollment results | worker thread start | one map insert per worker thread | a read per run |
| Stimulus | dedicated thread | none | 50ms of CPU, `n` × 1ms sleeps, one thread spawn, at most once per `cache_ttl`; no application worker stalls (2.6–3.1ms worst ticker gap vs 50ms for a worker stimulus) |
| Symbolization (options 1, 3) | blocking pool | none | first parse, 35–48ms and 15–22MB on ~50MB binaries, freed on drop |
| S3 marker | caller (1, 3) or worker (2) | none | one `PutObject`, once per `cache_ttl` |
| Pipeline shape, worker state (option 3) | builder, worker start and exit | none | a read |
| Probe slot (option 2) | worker loop | one atomic load per segment | one segment through every stage |

Nothing runs on application threads unless the app calls `run()`.

## Labeling smoke-test activity

| Activity | Reaches the trace? | How it's labeled |
|---|---|---|
| Stimulus CPU and scheduler samples | yes | thread name `dial9-smoke` (stimulus option B); unlabeled with option A |
| Symbolizer's first parse | yes, as CPU samples | thread name `dial9-symbolizer`, which exists today; production pays the same parse at the first CPU segment |
| Probe segment (option 2) | no | never enters the ring or the trace |
| S3 marker | no | `.txt` under `smoke-test/`, ignored by the viewer |
| Profiler start error | yes, segment metadata | key `{source}.start_error` |

A viewer can label or filter stimulus samples by thread name today, with no format change. A marker event through `Dial9Handle::record_event()` with the thread id and time window would also work. It adds a new event schema, which the self-describing format already supports, so it isn't a format change, but no viewer reads it. The thread name is enough.

## Follow up

- **Trigger from an env var.** For visibility in staging without code changes, an env var could run the smoke test once and log the report, like `DIAL9_SCHED_WAIT_SAMPLE_RATE` changes behavior today. It can't run in `build()`: `build()` is synchronous and runs before any runtime is attached. Run it from the first `attach_tokio_runtime`, spawned on the new runtime's `Handle`, since `tokio::spawn` panics on a thread without a runtime. Log only by default. With option 1 or 3, S3 needs its config from somewhere: the recorder doesn't keep one for the check.

  ```rust
  // In attach_tokio_runtime, after the runtime is built, once per process:
  if std::env::var_os("DIAL9_SMOKE_TEST").is_some() && FIRST_ATTACH.swap(false, SeqCst) {
      let tester = SmokeTester::new(self.clone(), SmokeTestConfig::builder().build());
      runtime.handle().spawn(async move {
          let report = tester.run().await;
          if report.is_healthy() {
              tracing::info!("dial9 smoke test:\n{report}");
          } else {
              tracing::error!("dial9 smoke test failed:\n{report}");
          }
      });
  }
  ```
- **Report a region fallback.** Needs `build_uploader` to return whether it fell back to `us-east-1`.
- **Untracking drops undrained per-thread samples.** `stop_tracking_current_thread` closes a thread's perf buffer without draining it (`perf_sampler.rs:306`). This already affects any tracked thread today: a worker's last scheduler samples at shutdown, and short-lived threads using `track_current_thread`. Draining before closing would fix it for everyone, independent of the smoke test.
- **Sched-wait sampling and write-back health.** Worth checking; out of scope for the first version.

## Testing

- **Recording**: a disconnected handle is `Failed`; paused is `Disabled(RecordingPaused)`; live with no profilers is `Ok` and healthy.
- **Sampling** (Linux, real recorder and runtime):
  - An idle runtime with `passive_window(Duration::ZERO)` passes through the stimulus.
  - The stimulus's samples carry the thread name `dial9-smoke`; a worker stimulus's don't.
  - Called from a runtime not attached to the recorder, the dedicated thread still proves sampling.
  - A 1ms ticker on a current-thread runtime stalls under 40ms with the dedicated thread, and about 50ms with a worker stimulus.
  - Called from a thread created before the profiler, perf reports `Unverified(OutsidePerfTree)`, not `Failed`.
  - Stimulus off gives `Unverified(StimulusOff)`, `is_verified()` false, and the next run re-checks.
  - `CpuProfilingConfig::with_ctimer_backend()` gives `Degraded(CtimerFallback)` and `cpu_backend() == Some("ctimer")`.
  - `sampling_interval(10)` still passes.
  - Off Linux, a start error of kind `Unsupported` gives `Disabled(Unsupported)`; any other start error gives `Failed`.
  - A worker whose enrollment fails makes `sched_sampling` `Failed`, even after a cached `Ok`. The enrolled count never exceeds the worker count.
- **Symbolization**: the probe resolves to its own name in debug, release, and fat-LTO builds. With options 2 and 3, a custom pipeline without `.symbolize()` fails, and one that compresses first fails. Run each scenario against every option, so a change in any option's behavior shows up.
- **Pipeline status** (option 3): stage names in pipeline order; `Initializing(Some(stage))` while a stage's `initialize()` hangs; `Running` after; `Stopped` after shutdown.
- **S3**: the fake-S3 harness, each scenario against every option: healthy, always failing, no S3 stage with a target, future-built client without a target, worker stuck initializing. A marker lands at the smoke key and nothing under `version=1/`; a second run overwrites it; a 403 gives the status, code, and message. With option 2 the tests go through a live recorder; real segments may upload during the test, and the probe's index `u32::MAX` tells them apart.
- **Worker** (option 2): answered in continuous and triggered mode and between segments; a second caller gets `Busy`; after a timeout on a stuck `initialize()`, the next call also times out; an init error gives `WorkerStopped`; a failing probe leaves the ring unchanged; `WriteBack` passes the probe through; a shuttle test for a probe racing shutdown (`shuttle_test!`).
- **Caching**: concurrent `run()` calls run at most one stimulus and one upload; `Failed` is re-checked; `cache_ttl(Duration::ZERO)` re-checks every run.
- Only one recorder can exist per process, so these tests need `cargo nextest` (one process per test).

## API stability

- `SmokeTestConfig`: `bon` builder, every field defaulted.
- `SmokeTester`: private fields.
- `SmokeTestReport`, `CheckStatus`, and the reason enums: `#[non_exhaustive]`.
- `perf-self-profile`: `CpuProfiler::effective_backend()`, `CpuProfiler::samples_seen()`, `SchedProfiler::samples_seen()`, `SchedProfiler::sampling_interval()`, `StartFailed<T>`, and a `smoke` module (`burn_thread_cpu`, `symbolization_probe`).
- `dial9-destinations-s3`: option 1 and 3 add `S3Config::smoke_test`; option 2 adds nothing public.
- `dial9-core`: option 2 adds `Dial9Handle::probe_pipeline`, its report and error types, `SegmentData::is_smoke_test()`, and the marker metadata keys. Option 3 adds read access to the pipeline shape and worker state.
- No trace format changes. `StartFailed<T>` adds a segment metadata key.

## Considered and dropped

- **An iteration-count burn**: the optimizer removes or shrinks it. See [Measurements](#measurements).
- **`yield_now` as the context switch**: it causes no OS switch.
- **Renaming a worker thread for the stimulus**: CPU sampling caches a thread's name from its first sample.
- **Checking for a debug-info section instead of resolving an address**: a binary can have the section and still fail to resolve.
- **A one-request channel for the option 2 probe**: a stuck worker turns every later run into `Busy`. See [Option 2 details](#option-2-details).
- **Having `SymbolizeProcessor` report the probe's name in metadata (option 2)**: decoding the payload works for any stage that adds a `SymbolTableEntry`, and keeps probe logic out of the processor.

## How the work splits

For the recommended options:

1. **Report types, `SmokeTester`, recording check** (`dial9-tokio-telemetry`), plus the `dial9` facade re-export.

Then, in any order:

2. **Sampling accessors and start failures** (`perf-self-profile`): counters, `effective_backend()`, `sampling_interval()`, `StartFailed<T>`, `burn_thread_cpu`.
3. **Enrollment results** (`dial9-tokio-telemetry`): latest result per worker id.
4. **Stimulus thread** (`dial9-tokio-telemetry`): `dial9-smoke` thread, `track_current_thread`, burn, sleeps.
5. **Symbolization probe** (`perf-self-profile`): `symbolization_probe`, resolve through `OfflineSymbolizer`, cache the result.
6. **S3 marker** (`dial9-destinations-s3`): `S3Config::smoke_test`.
7. **Pipeline shape and worker state** (`dial9-core`): stage names at build, worker state at start, init, exit.

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

**Samples per burn at 99Hz** (mean, zero-sample trials)

| Burn | perf | ctimer |
|---|---|---|
| 10ms | 0.8 (4/20) | 1.0 (0/20) |
| 30ms | 2.1 (2/20) | 3.0 (0/20) |
| 50ms | 3.9, min 3 (0/20) | 5.0 (0/20) |

**Context switches per stimulus, one-worker runtime**

| Stimulus | OS voluntary switches | Sched samples |
|---|---|---|
| `tokio::task::yield_now().await` | 0 | 0 |
| `std::thread::sleep(0)` | 0 | 0 |
| `tokio::time::sleep(1ms).await` (idle worker) | 1 | 1 |
| `std::thread::sleep(1ms)` | 1 | 1 |

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
