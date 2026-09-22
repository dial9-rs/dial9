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

The viewer has one canonical Vite/TypeScript UI (ADR-0004). Root `*.html`
files are thin entries at the public routes; page behavior lives under
`dial9-viewer/ui/src/pages/**`. Shared CJS-compatible logic remains in the
`ui/` root (for example `prefix_detect.js`) and is imported through the
`src/lib/**` seams; expose new shared exports through those seams. `creds.js`
and `url_state.js` remain classic browser-global contracts copied verbatim by
Vite. `/new/*.html` assets are compatibility redirects to those entries; there
is no second page implementation.

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
- For Rust behavior changes, run `cargo nextest run`. This needs `node` on `PATH`: tests that drive the JS trace tooling as a subprocess fail, not skip, without it.
- For final verification of Rust changes, run `cargo nextest run --stress-duration 20s`. The package is expected to have no flaky tests; report any apparent flake instead of ignoring it.
- **JS/HTML-only changes** (no `.rs` files touched, no trace format changes): you do NOT need to run the full Rust test suite or the stress test. Run the Vitest suites (`npm run test` in `dial9-viewer/ui/`, or a filtered `npx vitest run tests/core/<suite>.test.ts`) and a quick `cargo build -p dial9-viewer` to confirm `rust-embed` picks up any new files. Skip `cargo nextest` / stress run.
- **Adding a new JS/TS test:** write a Vitest suite — `dial9-viewer/ui/tests/core/*.test.{js,ts}` for suites over the shared core, `src/**/*.test.ts` for TypeScript modules. Vitest auto-discovers them and the `ui` CI job runs `npm run test`. If the suite must ALSO hold against a freshly regenerated demo trace in the DDB environment, add it to the `TRACE_SUITES` list in `scripts/e2e-trace-tests.sh` (run by the `trace-integrity` CI job). Exception: `dial9-viewer/ui/test_parser.js` stays a plain Node script — the Rust integration test `dial9-tokio-telemetry/tests/js_parser.rs` invokes it by filename with file arguments. See `dial9-viewer/ui/README.md`.
- Shuttle tests are NOT included in `cargo nextest run`. They require a separate invocation: `./scripts/test-shuttle.sh`. Always run this when modifying code under `#[cfg(all(test, shuttle))]` or the flush/source paths.
- **New shuttle scenarios:** use the `shuttle_test!` macro (`dial9-core/src/primitives.rs`) instead of hand-pairing `check_pct`/`check_uncontrolled_nondeterminism`. Its doc comment covers the `should_panic`/`expected=`/`replay=`/`flaky_sigabrt_determinism_only`/`verify_faults_triggered`/`default` modifiers.

### Skill delivery

Check that packaging materializes the skill symlinks as real files (`cargo
package --list` cannot show this):

```bash
rm -f target/package/dial9-[0-9]*.crate target/package/dial9-viewer-[0-9]*.crate
cargo package --allow-dirty --no-verify -p dial9 -p dial9-viewer
for c in dial9 dial9-viewer; do
  rm -rf /tmp/pkg-$c && mkdir -p /tmp/pkg-$c && tar -xzf target/package/$c-[0-9]*.crate -C /tmp/pkg-$c
  find /tmp/pkg-$c -path '*/skills/*' \( -type l -o -type f -empty \)
done
# no output: every packaged skill file is a regular, non-empty file
```

Check the two project shapes against a real Symposium. It needs a `cargo-agents`
built from symposium-dev/symposium newer than the published `symposium` 0.4.0
(the registry manifest format postdates it), network, and an isolated
`SYMPOSIUM_HOME` with the builtin registries off:

```toml
# $SYMPOSIUM_HOME/config.toml
[defaults]
symposium-recommendations = false
user-plugins = false

[[agent]]
name = "claude"

[[registry]]
name = "local"
path = "/path/to/registry"
auto-update = false
```

with the `dial9` entry from symposium-dev/recommendations copied to
`/path/to/registry/dial9/SYMPOSIUM.toml`. Then, in an empty crate:

- declaring `dial9 = "=X.Y.Z"` (at or above the first release carrying skills),
  `cargo agents sync` must install that release's set, identical to
  `~/.cargo/registry/src/*/dial9-X.Y.Z/skills/`;
- declaring only `dial9-tokio-telemetry = "=X.Y.Z"`, it must install the
  fallback set from the newest `dial9-viewer`.

A workspace whose members pin different `dial9` versions is seen by Symposium
as a single version per crate, the lowest one declared, and gets that
version's skills.

## Scope

- If you encounter unrelated or pre-existing warnings/failures, report them clearly and ask before fixing. Fix them immediately only when they block the requested work.

## Formatting and linting

- For Rust code changes, run `cargo fmt --check` and clippy. On Linux, run `cargo clippy --all-targets --all-features`. On non-Linux targets, run `cargo clippy --all-targets --features __nonlinux_all_features` instead. Report if you did not run them.
- **Preserve doc comments and inline comments.** When reviewing your diff, verify you have not accidentally deleted documentation comments (`///`, `//!`), inline explanatory comments (`//`), or module-level docs. Refactors that move code must carry all associated comments with it.
- **Keep comments concise.** State the fact or reason, not a narrative walkthrough. Avoid restating what the code already shows.

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

### Where the skills live

The skills are authored once, in `dial9-viewer/skills/`, and ship three ways:
the `dial9-viewer` package (Symposium's fallback edge), the `dial9-viewer`
binary (`agents skills` unpack, embedded by `build.rs`), and the `dial9` crate
package (Symposium's serving edge) through the `dial9/skills` symlink. Inside
the toolkit, `trace_parser.js`, `trace_analysis.js` and `decode.js` are
symlinks to the viewer UI modules and the trace-format JS decoder, so the
agent runs the same code as the viewer. Edit under `dial9-viewer/skills/` or
the linked sources; never add a second copy. `cargo package` materializes
every link as a real file (CI checks the tarball).

release-plz attributes a commit to a crate by the git paths under that crate's
directory that are also in its package file list. A skill edit is a
`dial9-viewer` change; `dial9` is released alongside because `release-plz.toml`
folds the viewer's commits into its changelog and version bump
(`changelog_include`), and it depends on the viewer besides. The viewer's
`include` lists `ui/trace_parser.js` and `ui/trace_analysis.js` for the same
reason: without that, a commit touching only them belongs to no crate.

Known limitations:

- Symposium serves version-matched skills only to a project that declares
  `dial9` at or above the floor. An older pin, a prerelease pin (a `>=X.Y`
  requirement never matches a prerelease) and a project that declares only
  `dial9-tokio-telemetry` take the fallback, the newest `dial9-viewer` skills.
  The fallback fires only while the registry entry keeps `dial9-tokio-telemetry`
  in its top-level `depends-on`: Symposium gates the entry on that list before
  evaluating any edge.
- A source-tree install (a project depending on `dial9` by `path` or `git`)
  silently drops symlinked files (symposium-dev/symposium#288): every skill
  installs, the toolkit's three linked libraries are missing, and a re-sync
  does not repair it. On a Windows checkout without symlink support
  `dial9/skills` is a text stub, so such an install delivers no skills at all.
  Installs from crates.io are unaffected.
- A project that declares both `dial9` and `dial9-viewer` directly and consents
  to the `dial9-viewer` crate offer receives each skill twice, under
  hash-suffixed names.
- Symposium offers the `dial9` crate for consent, since it ships `skills/`.
  Accepting changes nothing (the registry entry already installs it); declining
  is not honoured for chained plugins (symposium-dev/symposium#290).

Delivery through Symposium depends on the `dial9` entry of the
`symposium-recommendations` registry (symposium-dev/recommendations). See
"Skill delivery" under Testing for how to check it.

### Trace analysis skills

When analyzing dial9 traces or helping users use the viewer, discover the available trace-analysis skills with:

```bash
cargo run -p dial9-viewer -- agents
```

### Issue tracker

GitHub Issues on `dial9-rs/dial9-tokio-telemetry`. See `docs/agents/issue-tracker.md`.

### Domain docs

Single-context layout. See `docs/agents/domain.md`.
