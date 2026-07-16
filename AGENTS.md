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
the wire (not a compiled-in schema) to decode events.

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

## Viewer deep links (`/api/object` and `source_key`)

When building a viewer/flamegraph deep link in the UI that loads a specific
trace file, the `trace=/api/object?bucket=…&key=…` component's `key` must be a
**bucket-relative** key (e.g. `2026-07-15/1715/svc/host/boot/epoch-seg.bin.gz`),
NOT a fully-qualified `s3://{bucket}/{key}` URI. `/api/object` treats the `key`
verbatim, so passing an `s3://…` URI yields a **404**.

Beware: the folded backend stores a span/sample's `source_key` as the
fully-qualified `s3://{bucket}/{key}` form (that is the `full_key`
`decode_samples` was handed). So any UI code that turns a `source_key` into an
`/api/object` link must split off the `s3://{bucket}/` prefix first (parse the
bucket from the URI, keep the remainder as the key). See
`exemplarViewerUrl` in `dial9-viewer/ui/span_explorer.js`.

Diagnosing a broken deep link: a **404 is almost always a wrong key/endpoint**,
not missing credentials. Credentials (BYOC) live in `sessionStorage`, which the
browser copies to a tab opened via a normal same-origin link or `window.open`,
so a new viewer tab inherits them. Before blaming creds, print the exact
`/api/object?bucket=&key=` the link produces and `curl` it against the running
server — compare the `s3://…` key vs the bucket-relative key.

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
- **JS/HTML-only changes** (no `.rs` files touched, no trace format changes): you do NOT need to run the full Rust test suite or the stress test. Run the relevant JS tests under `dial9-viewer/ui/test_*.js` with `node <test>` and a quick `cargo build -p dial9-viewer` to confirm `rust-embed` picks up any new files. Skip `cargo nextest` / stress run.
- **Adding a new `dial9-viewer/ui/test_*.js` file:** CI does NOT auto-discover JS tests. You MUST register the new file in `scripts/e2e-trace-tests.sh` (the `trace-integrity` CI job runs that script), or it will never run in CI. See `dial9-viewer/ui/README.md`.
- Shuttle tests are NOT included in `cargo nextest run`. They require a separate invocation: `./scripts/test-shuttle.sh`. Always run this when modifying code under `#[cfg(all(test, shuttle))]` or the flush/source paths.

## Scope

- If you encounter unrelated or pre-existing warnings/failures, report them clearly and ask before fixing. Fix them immediately only when they block the requested work.

## Formatting and linting

- For Rust code changes, run `cargo fmt --check` and clippy. On Linux, run `cargo clippy --all-targets --all-features`. On non-Linux targets, run `cargo clippy --all-targets --features __nonlinux_all_features` instead. Report if you did not run them.
- **Preserve doc comments and inline comments.** When reviewing your diff, verify you have not accidentally deleted documentation comments (`///`, `//!`), inline explanatory comments (`//`), or module-level docs. Refactors that move code must carry all associated comments with it.

## Demo Trace

If you modify the trace format (event structure, encoding, parser, etc.), you MUST regenerate the demo trace:

```bash
./scripts/regenerate_demo_trace.sh
```

Or via Docker (no host Rust/AWS/Java needed — DDB Local runs as a sidecar):

```bash
./scripts/regenerate_demo_trace_docker.sh
```

Or manually:

```bash
rm -f dial9-viewer/ui/demo-trace.bin
cargo build --release -p metrics-service
AWS_PROFILE=your-profile cargo run --release -p metrics-service --bin metrics-service -- --trace-path sched-trace.bin --demo
cp sched-trace.*.bin dial9-viewer/ui/demo-trace.bin
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
