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

Already reachable: `Dial9Handle::with_source` can grab the `CpuProfiler`/`SchedProfiler` sources directly. If it returns nothing, that's `Disabled`, no extra code needed.

Sampling is random, so reading the counter right at startup can show zero even when everything's fine. The check polls the counter first: a busy service is usually already producing samples on its own, which proves the same thing a forced one would. Only if it's still zero after a bit, or the caller asks for it directly, does the check generate a CPU burst (and force a context switch for the sched profiler) to make sure.

Runs on a normal tokio task, not `spawn_blocking`: the sched sampler only turns on for worker threads, so the stimulus has to run on one.

The burst, when it runs, generates CPU load and forces a context switch, both landing in the trace as if they were real activity. Only happens when nothing was already sampling, or when explicitly requested.

Once this check reports `Ok`, that's remembered for an hour by default, configurable: a call within that window skips straight to `Ok`, no poll or burst. This makes calling `smoke_test()` from a recurring readiness endpoint safe: the burst fires at most once per window, and a real regression still gets caught the next time it expires.

```rust
SmokeTestConfig::builder()
    .cpu_sampling_cache_ttl(Duration::from_secs(300))  // re-verify every 5 minutes instead
    .build()
```

`SmokeTestConfig` can also suppress the burst entirely, separate from turning off the whole check:

```rust
SmokeTestConfig::builder()
    .cpu_sampling_burst(false)  // passive read only, never force a sample
    .build()
```

An idle service would then report `Failed`/`Degraded` even though it's fine, since nothing forced a sample to prove otherwise.

### Symbolization

`SymbolizeProcessor` lives inside the worker thread and can't be reached from outside. It wraps `OfflineSymbolizer`, which is public, so the check builds its own and resolves a known function's address.

Gated by its own `symbolize-processor` feature, separate from CPU profiling. Check it independently.

Result meanings:
- No symbol at all: bug.
- blazesym's `"[symbolize-failed]"`: bug.
- blazesym's `"[unknown]"`: `Ok`. This is the normal, correct result for a stripped release binary.
- The probe function's own name: `Ok`.
- Anything else: bug (offset or ASLR mismatch).

Runs on `spawn_blocking`, same as production: the symbolizer does blocking work and isn't `Send`.

The first call pays the cost of parsing the binary's debug info, which can take hundreds of milliseconds. Cache the `OfflineSymbolizer` in a `static OnceLock`, the same process-wide lazy-init pattern `DIAL9_SCHED_WAIT_SAMPLE_RATE` already uses, instead of rebuilding it each call. `spawn_blocking` also competes for a slot on tokio's blocking pool with the app's own blocking work; under a saturated pool this check can queue and add latency to the smoke test itself.

`SymbolizeProcessor` is a `SegmentProcessor`, moved into the worker thread and unreachable once built, same as the S3 uploader. There's no live counter to check whether the real pipeline already symbolized something. Caching removes the expensive part, the parse; what's left per call is small. A caller who wants zero cost can turn off the whole check with the existing flag.

### S3 upload

The uploader lives inside the worker thread too, and dial9-core can't reach it (crate boundary). The check builds a second, independent S3 client from whatever `S3Config` the caller passes in.

It uploads an object and never deletes it. Production doesn't have delete permission either (`s3:PutObject` + `s3:ListBucket` only), so testing for delete would test something the app doesn't need. The object goes to a fixed key by default, overwritten every run. Callers can point it elsewhere if they want history.

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

- Default is log-only. A library silently killing your process on a diagnostic failure would be a bad surprise. A stricter mode that exits the process on failure could exist as a separate opt-in, not the default.
- This doesn't replace calling `smoke_test()` directly. Only wiring it into a readiness endpoint lets a deploy pipeline act on the result.

Without that builder call, S3 gets skipped here, so this flow isn't actually zero code for S3, only for the other three checks. A fallback for this path: build a client from the AWS SDK's own credential chain (already handles env vars, IAM roles, profiles, nothing new needed there) plus one new env var for the bucket name, `DIAL9_SMOKE_TEST_S3_BUCKET`. Used only when no `S3SmokeTestTarget` was set up in code; the explicit, code-wired target wins whenever both exist. Nothing guarantees the env-derived client matches whatever credentials or endpoint production actually uses.

### Sched-wait sampling and write-back health

Sched-wait sampling coverage and local write-back health are worth checking too. Skipped for the first version to keep it scoped.

## Testing

- **Recording**: a plain unit test, one handle per state (paused, enabled, stopped), asserting each maps to the right status.
- **CPU sampling**: the passive-then-burst branching needs a fake, injectable counter to keep both paths (already sampling, still zero) deterministic. A Linux-gated integration test, following the existing `tests/fallback_detection.rs` pattern, covers the burst producing a sample under both perf and ctimer.
- **Symbolization**: a unit test asserting the second call doesn't re-parse (the cache holds), plus resolving the real probe function's address in a debug build, following the existing `symbolize_stack.rs` pattern.
- **S3 upload**: reuse the crate's existing fake-S3 harness (`fake_s3_client()`) to assert a successful `PutObject` and a failed one, deterministically, without touching real AWS.

## API stability

- `SmokeTestConfig`/`S3SmokeTestTarget`: builders, every field optional, safe to extend later.
- `SmokeTestReport`/`CheckStatus`/`S3SmokeTestStatus`: `#[non_exhaustive]`, safe to add variants later.
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
