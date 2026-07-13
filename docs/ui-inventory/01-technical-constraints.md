# Technical constraints for the UI migration

Stack-independent facts, extracted from the repo and the feature inventories
(`features/01-index-html.md`, `features/02-viewer-html.md`, `features/03-flamegraph-html.md`), that bound any
tech-stack or architecture choice for the dial9-viewer UI. Each constraint states its
source and what it eliminates. Hard constraints are non-negotiable properties of how
this crate is built, published, and tested; soft constraints are strong preferences
that a candidate stack must pay for violating.

## Hard constraints

### H1. The shipped UI is static files embedded at crate-build time; any JS build must run before packaging

`dial9-viewer` is a published crate installed via `cargo install --locked dial9`,
which compiles from source on end-user machines. The server embeds the UI with
rust-embed at compile time, verbatim from disk (`#[folder = "ui/"]`,
`dial9-viewer/src/server/mod.rs:44`). End-user machines have cargo, not Node/npm.

This does NOT forbid a build step. `cargo publish` packages whatever files exist
on disk at publish time, so a release-CI stage can run the JS build first and the
packaged crate then carries the built assets; `cargo install` consumers never need
Node. The prebuilt-binary path (`cargo-binstall`, `release.yml`) gets the same
treatment: build UI, then build binaries.

Consequences:
- A bundler is admissible. Its output must be plain static files inside the
  embedded dir, produced by CI before `cargo publish` / binary builds (or
  committed, but CI-built is cleaner: no generated code in git).
- `build.rs` still can never invoke npm (a git-source `cargo build` must not hard-
  require Node). The embed must tolerate a missing/stale dist gracefully for
  cargo-only builds, or the repo documents that UI work requires Node.
- Contributors building the UI from a git checkout need Node; cargo-only
  contributors (Rust changes) must remain unaffected.
- The release pipeline gains a JS stage that must be reproducible and pinned
  (lockfile), since it now produces shipped bits.

Eliminates: runtime/deploy-time bundling; any stack whose output is not plain
static files; unpinned/unreproducible UI builds in the release path.

### H2. Core logic modules must stay Node-runnable in source form

CI runs the JS test suite as plain `node <file>` scripts (Node 24; `ci.yml:168-175`
via `scripts/e2e-trace-tests.sh`, plus `stress-test.yml` running
`test_task_lifecycle.js` / `test_trace_integrity.js` directly against generated
traces). The shared modules (`decode.js`, `trace_parser.js`, `trace_analysis.js`,
`creds.js`, `heatmap.js`, `prefix_detect.js`) are dual browser/Node via
`module.exports` guards and are exercised by ~24 test files. Note their module
form: they are browser-global scripts with CommonJS export guards, NOT ES
modules.

Consequences:
- These modules are the frozen core: their source files keep their current form,
  locations, and test entry points. The migration consumes them; it does not
  port them.
- A bundler may COPY them into served page bundles (via CommonJS interop), but
  the on-disk source files remain the canonical, Node-tested form. Their CJS
  shape also means they are largely opaque to tree-shaking: they enter a bundle
  wholesale.
- The invariant is the SOURCE form, not the runner: tests may migrate to a
  different runner (e.g. Vitest) provided it consumes these unbundled source
  files directly and the files themselves stay unmodified.
- New code must be testable by a bare Node process (plain file import) or an
  equally cheap runner.

Eliminates: replacing the core source files with bundled-only artifacts or
breaking their `node test_x.js` entry points; any stack whose test path cannot
reach unbundled sources.

### H3. Canvas is the render core; the DOM is chrome

The viewer's hot path is imperative canvas rendering, not DOM reconciliation:
per-worker lane canvases with pixel-downsampling and run-length-coalesced fillRects
over millions of polls, RAF-throttled full-render and crosshair layers, DPR-aware
scaling, scrollbar-width compensation, and a shared time-panel layout invariant
(inventory `features/02-viewer-html.md` sections A, G, H, I; `panel_layout.js`). The same
holds for the index heatmap and the flamegraph.

Consequences:
- A reactive/vDOM framework can only ever manage the chrome (toolbars, panels,
  sidebar, popups, tables). The core render pipeline stays imperative canvas code
  regardless of stack. Framework benefit is capped at maybe 30% of the code.
- The chrome and the canvas share state (view window, selections, hover) with
  frame-rate update cadence; a stack must not put a slow reactivity layer between
  them.

Eliminates: nothing outright; caps the value of heavy UI frameworks and makes
"stays out of the canvas path's way" a first-class scoring criterion.

### H4. Trace-format backwards compatibility is untouchable

The decoder reads the self-describing on-wire schema; the JS viewer must tolerate
fields missing from old traces (`AGENTS.md`, Trace Format Backwards Compatibility).
This logic lives in the frozen core (H2). The migration must not re-implement
decode/parse semantics; behavior differences there are trace-corruption-grade bugs.

### H5. The disk-serve dev loop must survive

`dev-server` (and `dial9 serve --dev`) serves `ui/` from disk via `with_dev_ui_dir`
(`dial9-viewer/src/lib.rs:102-103`, `src/bin/dev_server.rs:102-103`): edit file,
refresh browser, no rebuild. Any stack with a compile step must provide an
equivalent loop (watch mode writing into the served dir) without making the
cargo-only path (no Node installed) unable to serve a working UI.

## Soft constraints

### S1. Zero JS dependencies today; every dep must pay rent

There is no `package.json`, no lockfile, no node_modules anywhere in the repo. The
entire UI (binary decoder included) is first-party. Rust deps are audited in CI
(`audit.yml`); there is no JS supply-chain machinery at all. Introducing npm
dependencies means creating that machinery (lockfile auditing, update policy,
vendoring decisions) from scratch.

### S2. Correctness outranks contributor familiarity (maintainer directive)

Contributor idiom familiarity is explicitly NOT a constraint: the maintainer
prioritizes correctness of the resulting UI over keeping the stack close to what
Rust-first contributors already know. Consequences for evaluation:

- Tools are scored by how many bug classes they remove (strict static typing,
  declarative rendering that cannot desynchronize DOM from state, exhaustive
  checks), not by ecosystem-idiom cost.
- JS-ecosystem idioms (JSX, tagged templates, compiler-checked components) are
  admissible whenever they buy correctness.
- Simplicity remains a tie-breaker only: between two equally correct options,
  pick the simpler; never trade correctness for simplicity.

### S3. Incremental, per-surface migration

The three entry pages (`index.html`, `viewer.html`, `flamegraph.html`) are
independent documents sharing modules. Migration must be able to land one surface
(or one extracted module) at a time with the rest untouched - already agreed as
incremental extraction, not greenfield. A stack that forces an all-at-once
paradigm switch inside a page loses points.

### S4. The actual disease is monolith + global mutable state

`viewer.html` is 7056 lines: ~970 lines CSS, ~6000 lines inline script in one
scope with roughly 68 top-level mutable `let` bindings, 9 script tags, and no
import graph (inventory 02, section W). Maintainability, not framework absence, is
the problem. The stack choice matters less than the architecture: module
boundaries, an explicit state model, and typed interfaces. Prefer the stack that
lets the architecture do the work with the least ceremony.

### S5. Evergreen-browser baseline already assumed

The code ships untranspiled modern JS (optional chaining, nullish coalescing,
`??`, ES2020+), so legacy-browser support is already not a goal. Native ES modules
in the browser are therefore available to any candidate stack.

### S6. Crate/asset weight

Every file under `ui/` is compiled into the `dial9-viewer` binary by rust-embed
and ships in the published crate - including the 3.4 MB `demo-trace.bin`. This is
an install-time and binary-size cost, NOT a page-load cost: the browser downloads
only the HTML + JS it references, and `demo-trace.bin` is fetched solely when the
demo is explicitly opened (`viewer.html?trace=demo-trace.bin`). Stack additions
that grow the embedded set (vendored runtime bundles, sourcemaps shipped into
`ui/dist`) inflate the binary and crate for every user, so they have a small but
real cost; release builds should embed only what pages actually reference.

Today the embed is already indiscriminate: `#[folder = "ui/"]` pulls in the ~24
`test_*.js` files, `test-traces/`, and READMEs alongside the served assets. The
chosen fix (02-architecture.md section 2.1) narrows the embed folder to the build
output dir (`ui/dist/`) so the binary carries exactly the servable set - no
exclusion lists to maintain.

## What this leaves open

Within these bounds the realistic candidate space is:

1. Vanilla ES modules, no build step, with JSDoc-based type checking in CI.
2. TypeScript + bundler (esbuild or Vite), no framework, CI-built `ui/dist/`.
3. Lightweight web components (Lit) for chrome, canvas core untouched.
4. Full framework + bundler (React/Svelte + Vite), CI-built dist.

The stack decision and consolidated migration plan are recorded in `docs/adr/0003-viewer-ui-migration.md`.
