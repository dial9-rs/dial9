# HANDOFF - T03 (CI + release pipeline build stage)

## STATUS

DONE locally. One DoD item (CI green on a real PR) is pending-CI by nature.

Branch: `ticket/T03-ci-release-ui-build` (base be2c009, the T02 tip).
Worktree: `/Users/facundo/code/wye/dial9-tokio-telemetry/.claude/worktrees/agent-a1fbd73895037a4ad`

Gate note: this worktree was originally cut from stale main (b776d27). The
orchestrator confirmed the diagnosis and authorized
`git switch -c ticket/T03-ci-release-ui-build be2c009` in place; verified
`git log --oneline -2` shows be2c009 as base and `dial9-viewer/ui/package.json`
exists before starting.

## COMPLETED (commit shas)

- e3c27fc build(viewer-ui): add vitest devDependency and test script
  - `dial9-viewer/ui/package.json`: `"test": "vitest run --passWithNoTests"`,
    vitest ^4.1.10 devDependency; lockfile updated via `npm install --save-dev vitest`.
    No vitest config added (T10's job).
- f17dc74 ci: add ui job (tsc, vitest, vite build); build UI before releases
  - `.github/workflows/ci.yml`: new `ui` job (Node 24 per trace-integrity
    precedent, npm cache keyed on `dial9-viewer/ui/package-lock.json`,
    working-directory `dial9-viewer/ui`): `npm ci` -> `npx tsc --noEmit` ->
    `npm run test` -> `npm run build`. `ui` added to `ci-pass` needs.
  - `.github/workflows/release.yml`: `npm ci && npm run build`
    (working-directory `dial9-viewer/ui`) inserted BEFORE the release-plz
    step in `release-plz-release`, and at the start of the
    `build-viewer-binaries` matrix job before `cargo build` (with
    `shell: bash` there - matrix includes windows-latest). Node 24 +
    setup-node pinned to the same sha as ci.yml. `release-pr.yml`
    untouched per spec.
- ea8e2c7 fix(viewer): package built ui/dist in the crate archive
  - `dial9-viewer/Cargo.toml`: switched `exclude` to an explicit `include`
    list. Required because cargo drops gitignored files (all of ui/dist/*
    except .gitkeep) unless `include` is specified - the crate would have
    shipped an EMPTY embedded UI.
  - `dial9-viewer/ui/README.md`: "UI development requires Node" section.
- (this commit) docs(T03): HANDOFF

## DoD EVIDENCE

### 1. CI green on a PR touching only ui/src, with `ui` in ci-pass needs

PENDING-CI (not locally checkable; no push allowed from this worktree).
Local proxies, all green:

- YAML validity + needs list (ruby -ryaml):

```
.github/workflows/ci.yml OK, jobs: fmt, clippy, build, nightly-gate, build-nightly,
  feature-check, check-docs, trace-integrity, ui, asan, ecs-sim, shuttle,
  semver-checks, package, ci-pass
ci-pass needs: fmt, clippy, build, feature-check, check-docs, trace-integrity,
  ui, asan, ecs-sim, shuttle, package
.github/workflows/release.yml OK, jobs: release-plz-release, build-viewer-binaries
```

- The exact `ui` job command sequence run locally from a clean install
  (Node v25.9.0 local vs Node 24 in CI):

```
$ npm ci                 -> ok ("found 0 vulnerabilities")
$ npx tsc --noEmit       -> ok (exit 0)
$ npm run test           -> "No test files found, exiting with code 0"  (vitest 4.1.10, --passWithNoTests)
$ npm run build          -> "vite v7.3.6 ... Copied 18 items. built in 35ms"
```

- actionlint: not installed locally, skipped.

### 2. cargo package produces an archive containing ui/dist with built assets

Ran after `npm run build`:

```
$ RUSTFLAGS="--cfg tokio_unstable" CARGO_TARGET_DIR=.../target cargo package \
    --no-verify --allow-dirty \
    -p dial9-trace-format-derive -p dial9-trace-format -p dial9-core \
    -p dial9-utils -p dial9-perf-self-profile -p dial9-macro \
    -p dial9-tokio-telemetry -p dial9-viewer
    Packaged 68 files, 4.8MiB (3.6MiB compressed)

$ tar -tzf target/package/dial9-viewer-0.4.0.crate | grep -c 'ui/dist/'
20        # .gitkeep + assets/dev-probe-*.js + 4 legacy pages + 12 core js
          # + flamegraph.css + demo-trace.bin
$ tar -tzf target/package/dial9-viewer-0.4.0.crate | grep -c 'node_modules\|ui/test_'
0
```

Notes on invocation:
- The ticket's single-crate form (`cargo package -p dial9-viewer
  --allow-dirty [--no-verify]`) fails with "no matching package named
  `dial9-core` found ... crates.io index" - dial9-core is an unpublished
  workspace dep, so a lone dial9-viewer package cannot resolve. This is
  PRE-EXISTING and independent of this change; the ci.yml `package` job
  packages all eight crates together for the same reason. I used that
  job's crate list with --no-verify.
- --no-verify used (full 8-crate verify build is slow locally; the ci
  `package` job runs the full verify on every PR). As a compile check with
  the new manifest, `cargo build -p dial9-viewer` (RUSTFLAGS
  --cfg tokio_unstable) finished clean in 1m03s.
- cargo package now warns "ignoring test `...` as tests/... is not
  included" (5x) plus the pre-existing dev-server warning: those targets
  are auto-stripped from the published manifest. Expected (see contents
  change below).

### 3. ui/README.md "UI development requires Node" note

Added as a `## UI development requires Node` section in
`dial9-viewer/ui/README.md` (commit ea8e2c7): dist/ is the served output,
npm ci / build / test / tsc commands, Node 24 in CI, cargo-only checkout
compiles with empty UI, end users never need Node.

## CRATE ARCHIVE CONTENTS CHANGE (intentional, in-scope)

Old archive (exclude-based): legacy ui sources, ~24 ui/test_*.js,
ui/demo-trace.bin + test-traces at ui root, tests/**, serve.py - and
ui/dist/.gitkeep ONLY (no built assets).
New archive (include list): build.rs, src/** (dev_server.rs still
unpublished), benches/**, skills/** + README_TELEMETRY.md (both read by
build.rs at compile time), README.md, ui/dist/**.
Dropped: ui sources/tests (DoD requires no ui/test_*.js), tests/** and
serve.py (the Rust integration tests reference ui-root files that no
longer ship, so packaging them would ship broken tests), design/,
benchmarks/ (excluded before too).

## REMAINING

- Pending-CI: open a PR touching only `dial9-viewer/ui/src` and confirm the
  `ui` job runs and `ci-pass` gates on it (DoD item 1). No push/PR allowed
  from this worktree.

## BLOCKERS

None.

## OBSERVATIONS (out of scope, not acted on)

- The ci.yml `package` job still runs cargo package WITHOUT an npm build,
  so the archive it verifies has an empty ui/dist (.gitkeep only). That is
  exactly the H1 "cargo-only checkout compiles" property and the job
  passes; the release path (release.yml) is the one that now builds dist
  first. If the maintainer wants CI's packaged archive to mirror the
  release archive byte-for-byte, that would be a follow-up.
- fmt/clippy not run: no .rs files touched (manifest, workflows, ui
  package metadata, README only). `cargo build -p dial9-viewer` run
  instead per AGENTS.md JS-change guidance.
