# Agent Guidelines

## API Design

This is a published library with backwards compatibility requirements. Follow
these rules for all public APIs:

- **Use builders for all configuration.** Never use positional arguments for
  config that may grow. Use `#[bon::builder]` (v3) to derive builders.
- **All builder fields should be private** with setter methods, so we can add
  fields without breaking changes.
- **Prefer `impl Into<String>` over `&str`** in builder setters for ergonomics.
- **Non-required fields must have defaults.** New fields added later must be
  optional or defaulted to avoid breaking existing callers.
- **Mark config structs `#[non_exhaustive]`** if not using `#[bon::builder]`,
  so adding fields is not a breaking change.
- **Think about semver hazards:** adding a required parameter, removing a
  public type, or changing a trait signature are all breaking. When in doubt,
  keep it private or behind a builder.

## Trace Format Backwards Compatibility

The trace format uses a self-describing schema: each event type's schema is
written to the wire before any events of that type. Decoders use the schema on
the wire (not a compiled-in schema) to decode events. A schema's *classifying*
annotations (e.g. the `dial9.role` annotations that mark a single-event span)
are likewise written before any event of that schema, so decoders classify in a
single pass without buffering — see `docs/design/single-event-spans.md`.

**Rules:**

1. **Adding new fields is always safe** — even non-optional ones. The decoder
   reads whatever fields the schema declares. Old traces simply won't have the
   new field in their schema, so it won't appear in the decoded output.

2. **Removing non-optional fields is NOT safe.** Old traces that contain the
   field will still declare it in their on-wire schema, and the decoder will
   attempt to read it.

3. **We only care about the JS decoder reading old traces.** Users always have
   a current decoder (the viewer), but may load old trace files. When you add a
   new non-optional field, the JS viewer code that accesses it must handle the
   field being `undefined` (because old traces won't have it):

   ```js
   // Good — gracefully handles old traces missing the field
   const workerId = v.worker_id != null ? num(v.worker_id) : undefined;

   // Bad — will throw or produce NaN on old traces
   const workerId = num(v.worker_id);
   ```

4. **Rust decoder backwards compat is not a concern.** We don't need to worry
   about old Rust decoders reading new traces.

## Coding practices

**Do not hide missing data or errors with plausible defaults like `unwrap_or(0)`
or `unwrap_or_default()`.** Use an explicit semantic default only when it is
truly valid for the domain, such as an empty collection. Otherwise, handle the
actual condition: propagate the error, return `Option`, log and skip, or panic if
the invariant is truly unrecoverable.

Avoid dropping an error without logging it. Use `tracing` for logging.
```
let _ = ...
```

**Rate-limit logging that can fire repeatedly from loops or high-volume paths.** Any repeated `warn!`/`error!` reachable from a background task loop, retry loop, or other unbounded error path should be wrapped in `rate_limited!`:
```rust
rate_limited!(Duration::from_secs(60), {
    tracing::warn!("...: {e}");
});
```
Unguarded logging in loops causes log spam that degrades observability and can itself become a performance problem. One-time paths (startup, shutdown, per-thread init) are exempt.

## Viewer UI

The viewer UI is mid-migration (ADR-0004): every page exists in two versions —
a **legacy** one (inline `<script>` in `dial9-viewer/ui/*.html`, e.g.
`index.html`) served at its canonical URL, and a **new** Vite/TypeScript one
(`dial9-viewer/ui/src/pages/**`, served under `/new/…`) that is now the
default. **Make behavior changes in the new UI only** (`src/`). Do NOT edit the
legacy pages/scripts — they are frozen for the migration and are not what users
load. Shared logic lives in the frozen-core modules at the `ui/` root (e.g.
`prefix_detect.js`), imported into the new UI through the `src/lib/**` seams;
change those when the behavior is genuinely shared, and expose new exports via
the seam rather than reaching into the legacy pages.

## Testing

### Local viewer server

For local server testing, run the viewer from the repository root with:

```bash
cargo run -p dial9-viewer -- serve --port 3003 --local --dev
```

When testing on-demand aggregation, prefer a release build so Parquet encoding
and trace decoding behave at representative speed:

```bash
cargo run --release -p dial9-viewer -- serve --port 3003 --local --dev
```

This is the recommended workflow: `--local` enables readable workstation logs,
`--dev` serves UI assets directly from `dial9-viewer/ui`, and omitting
`--agg-output-bucket` keeps on-demand S3/BYOC aggregate rollups in a
process-local temporary directory. Source credentials therefore need only read
access, and the temporary rollups are removed when the server exits. Open
`http://127.0.0.1:3003/`; use `/tmp/dial9-viewer-3003.log` when running it in the
background during agent-driven testing.

- Behavior changes should include focused tests that fail without the change; if tests are not practical, state why.
- For Rust behavior changes, run `cargo nextest run`.
- For final verification of Rust changes, run `cargo nextest run --stress-duration 20s`. The package is expected to have no flaky tests; report any apparent flake instead of ignoring it.
- **JS/HTML-only changes** (no `.rs` files touched, no trace format changes): you do NOT need to run the full Rust test suite or the stress test. Run the Vitest suites (`npm run test` in `dial9-viewer/ui/`, or a filtered `npx vitest run tests/core/<suite>.test.ts`) and a quick `cargo build -p dial9-viewer` to confirm `rust-embed` picks up any new files. Skip `cargo nextest` / stress run.
- **Adding a new JS/TS test:** write a Vitest suite — `dial9-viewer/ui/tests/core/*.test.ts` for suites over the frozen core, `src/**/*.test.ts` for new TS modules. Vitest auto-discovers them and the `ui` CI job runs `npm run test`. If the suite must ALSO hold against a freshly regenerated demo trace in the DDB environment, add it to the `TRACE_SUITES` list in `scripts/e2e-trace-tests.sh` (run by the `trace-integrity` CI job). Exception: `dial9-viewer/ui/test_parser.js` stays a plain Node script — the Rust integration test `dial9-tokio-telemetry/tests/js_parser.rs` invokes it by filename with file arguments. See `dial9-viewer/ui/README.md`.
- Shuttle tests are NOT included in `cargo nextest run`. They require a separate invocation: `./scripts/test-shuttle.sh`. Always run this when modifying code under `#[cfg(all(test, shuttle))]` or the flush/source paths.

## Scope

- If you encounter unrelated or pre-existing warnings/failures, report them clearly and ask before fixing. Fix them immediately only when they block the requested work.

## Formatting and linting

- For Rust code changes, run `cargo fmt --check` and clippy. On Linux, run `cargo clippy --all-targets --all-features`. On non-Linux targets, run `cargo clippy --all-targets --features __nonlinux_all_features` instead. Report if you did not run them.
- **Preserve doc comments and inline comments.** When reviewing your diff, verify you have not accidentally deleted documentation comments (`///`, `//!`), inline explanatory comments (`//`), or module-level docs. Refactors that move code must carry all associated comments with it.

## Demo Trace

If you modify the trace format (event structure, encoding, parser, etc.), the metrique sink's emitted event shape, or the demo app's `RequestMetrics` entry, you MUST regenerate the demo trace; `trace_integrity.test.ts` asserts on its contents. Regenerate on a host with `perf_event_paranoid <= 1` so sched events survive, with `DIAL9_SCHED_WAIT_SAMPLE_RATE=1` and CPU load so sched-wait samples are captured (the script validates this). Afterwards refresh the demo-pinned anchors in `flamegraph_search.test.ts` if they fail.

```bash
./scripts/regenerate_demo_trace.sh
```

Or via Docker (no host Rust/AWS/Java needed — DDB Local runs as a sidecar):

```bash
./scripts/regenerate_demo_trace_docker.sh
```

Or manually:

```bash
rm -rf dial9-viewer/ui/public/demo-trace.bin sched-traces
cargo build --release -p metrics-service
AWS_PROFILE=your-profile cargo run --release -p metrics-service --bin metrics-service -- --trace-path sched-traces --demo
cp sched-traces/trace.*.bin dial9-viewer/ui/public/demo-trace.bin
```

The demo trace is used for:
- Live demos on the hosted viewer
- Documentation screenshots
- Testing the viewer with real data

Failing to update it will cause the viewer to fail when loading the demo.

## Repository management

- Only when explicitly asked to open or manage PRs: do not stack PRs (PR B targeting PR A's branch). The merge queue rewrites commits, so stacked PRs always end up with merge conflicts. Instead, wait for the first PR to merge, then rebase the second onto `main`.

## Agent skills

### Trace analysis skills

When analyzing dial9 traces or helping users use the viewer, discover the available trace-analysis skills with:

```bash
cargo run -p dial9-viewer -- agents
```

### Issue tracker

GitHub Issues on `dial9-rs/dial9-tokio-telemetry`. See `docs/agents/issue-tracker.md`.

### Domain docs

Single-context layout. See `docs/agents/domain.md`.
