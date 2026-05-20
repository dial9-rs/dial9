# Progress: on_thread_start/on_thread_stop callback chaining (#297)

## Problem
`TracedRuntimeBuilder::build_and_start` calls `builder.on_thread_start()` internally, replacing any user-provided callback. Users cannot combine dial9 instrumentation with their own `on_thread_start`/`on_thread_stop` hooks.

## Solution
Added `on_thread_start` and `on_thread_stop` methods to `TracedRuntimeBuilder` that store user callbacks as `Option<Arc<dyn Fn() + Send + Sync>>`. These are threaded through `register_hooks` and `attach_runtime`, where they are chained with dial9's internal hooks:

- **on_thread_start**: user callback fires *after* dial9's setup (TelemetryHandle installed, CPU profiling registered)
- **on_thread_stop**: user callback fires *before* dial9's teardown (handle cleared, profiling unregistered)

This ordering ensures the user callback can use `TelemetryHandle::current()` in `on_thread_start` and that dial9's cleanup doesn't interfere with user teardown.

## Files changed
- `dial9-tokio-telemetry/src/telemetry/recorder/mod.rs` — fields, setters, plumbing
- `dial9-tokio-telemetry/tests/thread_hook_callbacks.rs` — integration test
- `dial9-tokio-telemetry/tests/usage_patterns.rs` — fixed unused import (pre-existing)

## Verification
- `cargo fmt --check` ✓
- `cargo clippy --all-targets --all-features` ✓
- `cargo nextest run -p dial9-tokio-telemetry` — 354/354 pass
- Stress test: 1701 iterations in 20s, zero failures
