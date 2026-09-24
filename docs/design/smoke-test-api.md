# Smoke-Test API

## What this is

Services using dial9 have no way to check, before going live, that telemetry works. Three things can fail silently right now:

- CPU sampling can fall back to a weaker backend, or capture nothing, if the OS blocks `perf_event_open`.
- Symbolization can produce unresolved stack traces when a release binary is stripped of debug info.
- S3 upload can fail on IAM or network problems, but you only find out when a segment tries to upload.

`smoke_test()` checks all three (plus whether recording is even running) and returns a pass/fail report. Call it from your own readiness endpoint so a broken deploy fails fast instead of failing quietly in production.

## How you call it

```rust
handle.smoke_test(SmokeTestConfig::builder()
        .maybe_s3(s3_target)
        .build())
    .await
```

It's a method on `Dial9Handle`, added through an extension trait (`Dial9HandleSmokeTestExt`), same pattern as the other `Dial9Handle` extensions already in the codebase.

## The report

Each check comes back as one of four states:

- `Ok`: working.
- `Degraded { detail }`: working, but not fully (e.g. using ctimer instead of perf).
- `Disabled`: not compiled in or not configured. Doesn't count as a failure.
- `Failed { detail }`: should work, doesn't.

`report.is_healthy()` is false only if something is `Failed`. All-`Disabled` still counts as healthy.

Checks: `recording`, `cpu_sampling`, `symbolization`, `s3_upload`.

Each check can be turned off in `SmokeTestConfig`. A turned-off check reports as `Disabled`, same as "not configured."

```rust
SmokeTestConfig::builder()
    .cpu_sampling(false)
    .symbolization(false)
    .build()
```

## The checks

### Recording

Uses `is_stopped()`/`is_enabled()` on the handle, both already exist:

```rust
if is_stopped() { Failed }
else if !is_enabled() { Disabled }
else { Ok }
```

### CPU sampling

`Dial9Handle::with_source` returns the `CpuProfiler`/`SchedProfiler` sources. If it returns nothing, the check is `Disabled`.

Neither source exposes what the check needs yet. `effective_backend` is a private field on `CpuProfiler`, visible only as an entry in `Source::segment_metadata()` output. `drain()` and `for_each_sample` on both profilers are `pub(crate)` and consume samples as they read them; there is no counter to read. This section assumes the counters and accessors from phase 1 of "How the work splits".

Sampling is random, so the counter can read zero at startup even when sampling works. The check polls the counter first: a busy service already produces samples, which proves the same thing a forced sample would. If the counter is still zero after a short wait, or the caller asks for it, the check runs a CPU burst.

The backend and the sample count combine into one status, and `Failed` takes precedence over `Degraded`. A ctimer-backed service that produces samples is `Degraded`. A service that produces no samples, even after the burst, is `Failed`, whatever the backend.

`SchedProfiler` has no fallback backend: if `perf_event_open` fails, it never registers, and the check is `Disabled`. Each worker thread enrolls on its first poll, and enrollment can fail per thread with nothing exposing the failure. Forcing a context switch on a worker is the only way to check that worker's enrollment.

Runs on a normal tokio task, not `spawn_blocking`: the sched sampler only turns on for worker threads, so the stimulus has to run on one.

**Known limitation:** the burst is one task, and it runs on whichever worker the scheduler picks, so it checks that worker's enrollment only. A service with 3 of 4 workers unenrolled can report `Ok` if the task lands on the enrolled one. Workers enroll per thread on their first poll (see the doc comment in `sched_non_tokio.rs`), each failure is a separate `Err` (see the `rejects_zero_period` test in `sched_triggers.rs`), and nothing aggregates them. Checking every worker would mean spawning about `num_workers` tasks, with no guarantee of hitting each one, because tokio can't target a specific worker. Out of scope for the first version, like sched-wait sampling and write-back health.

The burst, when it runs, generates CPU load and forces a context switch, both landing in the trace as if they were real activity. Only happens when nothing was already sampling, or when explicitly requested.

**The stimulus must survive release optimizations.** A PoC busy loop that took 324ms in a debug build took 83ns in release: the optimizer removed the work. Production runs release builds. The stimulus has to prevent this, for example with `black_box` on the loop variable every iteration (not only on the result), or with syscalls or volatile writes. Otherwise the burst produces almost no CPU load and no samples.

Once this check reports `Ok`, that's remembered for an hour by default, configurable: a call within that window skips straight to `Ok`, no poll or burst. This makes calling `smoke_test()` from a recurring readiness endpoint safe: the burst fires at most once per window, and a real regression still gets caught the next time it expires.

```rust
SmokeTestConfig::builder()
    .cpu_sampling_cache_ttl(Duration::from_secs(300))  // re-verify every 5 minutes instead
    .build()
```

`Duration::ZERO` disables the cache, so every call re-verifies. On a busy service this costs nothing extra, because the poll finds new samples without a burst. On an idle service every call runs a burst. The default TTL exists to avoid that.

`SmokeTestConfig` can also suppress the burst entirely, separate from turning off the whole check:

```rust
SmokeTestConfig::builder()
    .cpu_sampling_burst(false)  // passive read only, never force a sample
    .build()
```

An idle service would then report `Failed`/`Degraded` even though it's fine, since nothing forced a sample to prove otherwise.

### Symbolization

`SymbolizeProcessor` lives inside the worker thread and can't be reached from outside. It wraps `OfflineSymbolizer`, which is public, so the check builds its own.

`OfflineSymbolizer::symbolize()` takes an encoded trace segment with a `StackFrames` event and returns encoded symbol-table bytes. To resolve one probe address, the check encodes a segment with a single `StackFrames` event, calls `symbolize()`, and decodes the matching `SymbolTableEntry`.

The crate has two simpler functions: `resolve_symbol_with_maps(addr, &Symbolizer, &maps)` (used in `perf-self-profile/tests/symbolize_stack.rs`) and `resolve_symbol(addr)`, which adds a cache. Neither fits. `resolve_symbol`'s cache is `thread_local!`, and `spawn_blocking` runs each call on whichever pool thread is free, so most calls would re-parse the binary. `OfflineSymbolizer` owns one dedicated thread, caches across calls from any thread, and is the path production uses.

Gated by its own `symbolize-processor` feature, separate from CPU profiling. Check it independently.

Result meanings (match by prefix: both bracketed forms end with the address, e.g. `"[unknown] 0x7f2a1c003420"`):
- No symbol at all: bug.
- blazesym's `"[symbolize-failed]"`: bug. The blazesym call failed, e.g. it couldn't read the ELF.
- blazesym's `"[unknown]"`: `Ok`. The call succeeded but the address has no symbol. This is the normal result for a stripped release binary.
- The probe function's own name: `Ok`.
- Anything else: bug (offset or ASLR mismatch).

Runs on `spawn_blocking`, same as production: the symbolizer does blocking work and isn't `Send`.

The first call parses the binary's debug info. The source comment on this path (issue #462) says this typically takes several hundred milliseconds; this design didn't re-measure it. Cache the `OfflineSymbolizer` in a `static OnceLock`, the same lazy-init pattern `DIAL9_SCHED_WAIT_SAMPLE_RATE` uses, instead of rebuilding it on each call. `spawn_blocking` also shares tokio's blocking pool with the app: when the pool is saturated, the check waits for a slot and the smoke test takes longer.

Existing tests cover the cache. `offline_symbolizer_reuses_one_blazesym_across_segments` in `offline_symbolize.rs` asserts one blazesym construction across three `symbolize()` calls. The same file exposes `symbolizer_constructions()`/`state_constructions()` for cache-hit assertions without timing, which this check's test can use (see Testing). The worker loop wraps each resolve in `catch_unwind`, so a blazesym panic doesn't lose the cache or kill the thread, and the caller can retry.

There's no cheaper substitute for a full resolve. Checking for a debug-info section doesn't prove the same thing: a binary can have the section and still fail to resolve. The cache already limits the parse to once per process, so what's left to choose is when the parse happens.

Production's `OfflineSymbolizer` builds lazily because it must be created from the worker thread to inherit its profiling. The smoke test's symbolizer has no such constraint and could warm up at `build()` time. It stays lazy by default: `build()` doesn't know whether the caller will run `smoke_test()` with `symbolization` on, so warming up there could pay the parse cost for nothing. Callers who need a fast first call, e.g. a readiness endpoint with a tight timeout right after a deploy, can opt into pre-warming.

`SymbolizeProcessor` is a `SegmentProcessor`, moved into the worker thread and unreachable once built, same as the S3 uploader. There's no live counter to check whether the real pipeline already symbolized something. Caching removes the expensive part, the parse; what's left per call is small. A caller who wants zero cost can turn off the whole check with the existing flag.

### S3 upload

The uploader also lives inside the worker thread, and dial9-core can't reach it (crate boundary). The check needs a client and an `S3Config` for the bucket, key, and timeout, but `S3Config`'s accessors (`bucket()`, `region()`, etc.) are `pub(crate)`, so `dial9-tokio-telemetry` can't read them. The `PutObject` therefore lives in `dial9-destinations-s3` as `S3Config::smoke_test` (phase 3 below). `S3SmokeTestTarget` only carries the `(S3Config, Client)` pair to it.

It uploads an object and never deletes it. Production doesn't have delete permission either (`s3:PutObject` + `s3:ListBucket` only), so testing for delete would test something the app doesn't need. The object goes to a fixed key by default, overwritten every run. Callers can point it elsewhere if they want history.

**S3 uses the same TTL cache as CPU sampling.** Without it, a readiness endpoint calling `smoke_test()` would upload an object on every poll, with network cost and S3 request charges. `s3_upload_cache_ttl` defaults to an hour, and `Duration::ZERO` disables it, like `cpu_sampling_cache_ttl`. Within the window, a call returns the cached result without a `PutObject`.

**Error detail must come from `ProvideErrorMetadata`, not `Display`.** `err.to_string()` on a failed `PutObject` prints `"service error"` for every failure. `ProvideErrorMetadata::code()`/`.message()` return the actual reason (e.g. `"InternalError"` / `"permanent failure"`); use them for `Failed { detail }`. Checked with the fake-S3 harness's failure injection.

**How the caller supplies the S3 config:** one new builder method, next to `with_s3_uploader_client`:

```rust
fn with_s3_uploader_client_and_smoke_target(
    self,
    config: S3Config,
    client: aws_sdk_s3::Client,
) -> (Self, S3SmokeTestTarget);
```

It hands back the builder (to keep chaining as normal) and an `S3SmokeTestTarget` built from the same config/client. Save that target, pass it to `smoke_test()` later. The check's client is a separate instance from production's, built from the same config and credentials.

Two things considered and dropped:
- Grabbing the already-running uploader's client back out. Can't: it's built lazily, inside the worker thread, with no way to hand it back without adding new cross-thread plumbing to the production upload path.
- A general builder collecting config for any future check. Overkill right now: S3 is the only check that needs caller config at all.

## Trace noise

### CPU sampling

The burst only runs when nothing was already sampling, or when explicitly requested, and at most once per cache window (an hour by default). Each run adds about 5 samples to the trace.

This check has to run on a worker thread tokio manages: the sched sampler and the ctimer fallback only enroll threads tokio spawns. Renaming the worker thread for the burst has no effect either, since CPU sampling caches a thread's name from its first sample and never updates it, well before any smoke test runs.

If it needed to be identifiable, a plain event (`Dial9Handle::record_event()`, already public) recording the thread id and time window could let a viewer label matching CPU samples as "smoke test" without excluding them. Not part of this design: no viewer supports it, and it would be a new event type, a trace format addition (safe under the existing backwards-compat rules, but still an addition, not "no change").

### Symbolization

On its first call, it spawns its own OS thread to parse the binary's debug info, which can take hundreds of milliseconds, and CPU sampling picks that up like any other thread's activity. Only happens once, since the `OfflineSymbolizer` gets cached after that.

This one's already named (`"dial9-symbolizer"`), and CPU samples already carry the thread name they came from, so a viewer could label it today. No viewer does yet.

## Follow up
### Trigger it from an env var

`smoke_test()` still needs the app to call it and do something with the result. For visibility in staging with zero code changes, dial9 already has a precedent: `DIAL9_SCHED_WAIT_SAMPLE_RATE` tunes behavior from an env var alone.

Same idea here: `build()` can check an env var and, if set, run the smoke test once on its own and log the result. If the app already called `with_s3_uploader_client_and_smoke_target`, the recorder has the `S3SmokeTestTarget` in hand and S3 gets checked too.

`build()` is synchronous everywhere in this codebase, and `smoke_test()` is async (network calls, `spawn_blocking`). `build()` can't await it: blocking on a runtime from inside one can deadlock. So `build()` spawns a detached task and returns immediately, and the task logs the result when it finishes.

A PoC checked this. `attach_tokio_runtime` builds and owns its `Runtime`, and works when `build()`/`attach()` runs outside any runtime (the existing test `tokio_handle_spawns_on_correct_runtime_from_outside` covers this). The problem is the free function `tokio::spawn(...)`: it calls `Handle::current()` and panics when the calling thread has no runtime, even right after the runtime was built. Spawn on the `Handle` that `attach_tokio_runtime` returns instead (`handle.spawn(...)`), which works from any thread. `Dial9TokioHandle::spawn` already does this, so the caller needs no runtime context.

- Default is log-only. A library silently killing your process on a diagnostic failure would be a bad surprise. A stricter mode that exits the process on failure could exist as a separate opt-in, not the default.
- This doesn't replace calling `smoke_test()` directly. Only wiring it into a readiness endpoint lets a deploy pipeline act on the result.

Without that builder call, S3 is skipped, so this flow needs code for S3; the other three checks need none. A fallback: build a client from the AWS SDK's default credential chain (env vars, IAM roles, profiles) and read the bucket name from a new env var, `DIAL9_SMOKE_TEST_S3_BUCKET`. It applies only when no `S3SmokeTestTarget` was set in code; a target set in code wins. The env-derived client may not match the credentials or endpoint production uses.

### Sched-wait sampling and write-back health

Sched-wait sampling coverage and local write-back health are worth checking too. Skipped for the first version to keep it scoped.

## Testing

- **Recording**: a plain unit test, one handle per state (paused, enabled, stopped), asserting each maps to the right status.
- **CPU sampling**: the passive-then-burst branching needs a fake, injectable counter to keep both paths (already sampling, still zero) deterministic. A Linux-gated integration test, following the existing `tests/fallback_detection.rs` pattern, covers the burst producing a sample under both perf and ctimer.
- **Symbolization**: a unit test asserting the second call doesn't re-parse, using the `symbolizer_constructions()`/`state_constructions()` helpers in `offline_symbolize.rs` (no timing checks). Plus a test resolving the probe function's address in a debug build, following `symbolize_stack.rs`.
- **S3 upload**: reuse the crate's existing fake-S3 harness (`fake_s3_client()`) to assert a successful `PutObject` and a failed one, deterministically, without touching real AWS.

## API stability

- `SmokeTestConfig`/`S3SmokeTestTarget`: builders, every field optional, safe to extend later.
- `SmokeTestReport`/`CheckStatus`: `#[non_exhaustive]`, safe to add variants later. S3 upload uses `CheckStatus` like the other checks: details such as which step failed or the retry count fit in the `detail` string of `Failed`/`Degraded`.
- `Dial9HandleSmokeTestExt`: sealed, safe to add methods later.
- No trace format changes for the checks described above. The optional marker event under Trace Noise, if it's ever built, would be one: a new event type, additive and safe, but a change.

## How the work splits

First: **report types and `smoke_test()` itself** (`dial9-tokio-telemetry`), wired up with just the recording check. Recording needs no new plumbing (`is_stopped()`/`is_enabled()` already exist), so this gets `SmokeTestConfig`, `SmokeTestReport`, `Dial9HandleSmokeTestExt`, and the `dial9` facade re-export working end to end fastest.

Then, in parallel, any order:

1. **CPU sampling liveness** (`perf-self-profile`): counters and accessors on `CpuProfiler`/`SchedProfiler`.
2. **Symbolization self-test** (`perf-self-profile`): new module, resolves a probe function's address.
3. **S3 marker upload** (`dial9-destinations-s3`): `S3Config::smoke_test`, `S3Uploader::put_marker_object`.
4. **The S3 builder addition** (`dial9-tokio-telemetry`): `with_s3_uploader_client_and_smoke_target`.

Each plugs into the report as it's ready.

Follow up:
- Env-var boot check
- Sched-wait sampling
- Write-back health
