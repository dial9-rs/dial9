# ADR-0003: Viewer UI migration - consolidated plan

- **Status:** pending
- **Date:** 2026-07-02

## Context

The dial9-viewer UI (three monolithic pages: `viewer.html`, `index.html`,
`flamegraph.html`) is being migrated to a scalable, maintainable, corrected
foundation.

Ground rule: lose nothing. The current functional surface is the contract. This ADR records the consolidated decisions; the detail lives in the working documents:

| Document                                        | Role                                                                                                                                           |
| ----------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `docs/ui-inventory/features/01..03-*.md`        | Functional contract: every feature of the three pages, with access paths and source anchors (index page validated live against the running UI) |
| `docs/ui-inventory/01-technical-constraints.md` | Binding constraints (H1-H5 hard, S1-S6 soft)                                                                                                   |
| `docs/ui-inventory/02-architecture.md`          | Non-functional requirements and target architecture                                                                                            |
| `docs/ui-inventory/03-performance-findings.md`  | Measured structural performance issues and the design rules they yield                                                                         |
| `docs/ui-inventory/04-ux-findings.md`           | UX audit: 23 verified findings (8 structural, 8 keyboard, 7 feedback), genre-gap table, works-well list                                        |
| `docs/ui-inventory/mocks/`                      | Runnable design mocks (3 layout concepts + interactive keyboard model); see its README                                                         |

## Decisions

### 1. Functional contract

The feature inventories are the acceptance baseline: at the end of the migration every inventory row is observably intact or consciously retired with sign-off. Four known defects are fixed structurally: the dead raw-table column sort (sortable-table component), `parseKey`'s silent mislabel of unknown key layouts (explicit `known | unknown` discriminant), the dial9 bucket-filter lockout (config-driven predicate), and the date-ambiguous time-only heatmap axis (date+time on day-crossing spans).

### 2. Stack: TypeScript + Vite, no framework

Note: When I say DOM chrome I mean the HTML-element surfaces around the canvases: toolbar, sidebar, popups, tooltips, tables.

Chosen over vanilla ESM (no library access), raw esbuild (no integrated test runner), and component frameworks (their benefit is capped at the DOM chrome since the hot path is imperative canvas in every candidate, while they cost more in test continuity, install fit, and dependency size). Vite supplies dev transforms, Rollup production builds, HMR, and Vitest.

- Strict TypeScript everywhere (`strict: true`, `noUncheckedIndexedAccess`); erasable-syntax subset so Node can execute test files directly.
- No application framework. DOM chrome uses a micro declarative templating library (lit-html class: state -> template, no component runtime) so chrome cannot desynchronize from state; state is a small first-party typed store.
- npm dependencies allowed; every runtime dependency justified in its PR; lockfile-pinned builds.
- Escape hatches: dropping to raw esbuild changes only build config and test runner; module boundaries, types, and store carry over.

### 3. Project shape and packaging

`dial9-viewer/ui/` becomes a self-contained standard Vite multi-page project:

```
dial9-viewer/ui/
  package.json, package-lock.json     # toolchain; node_modules/ gitignored
  vite.config.ts, tsconfig.json
  index.html, viewer.html, flamegraph.html   # Vite MPA entries (thin shells)
  public/                             # verbatim assets, copied into dist/
  src/
    pages/                            # one entry module per page
    components/                       # DOM components (the chrome)
      canvas/                         # canvas components (lanes, heatmap, panels)
    lib/
      canvas/                         # shared drawing math (layout, DPR, downsampling)
      interact/                       # pointer/keyboard/wheel state machines
      trace/                          # typed boundary around the frozen core
    store/                            # typed state slices + scheduler
    types/                            # trace/state shapes; .d.ts for frozen core
    styles/
  decode.js, trace_parser.js, ...     # frozen core + test_*.js (ui/ root, not embedded)
  dist/                               # build output (gitignored) = rust-embed folder
```

Per `02-architecture.md` section 2.1:
The rust-embed folder narrows from `ui/` to `ui/dist/`: the binary embeds exactly the built servable assets, never sources, tests, or `node_modules` (this also stops embedding the ~24 test files shipped today). Release CI runs `npm ci && npm run build` before `cargo publish` / binary builds, so `cargo install` users and binstall consumers never need Node; `build.rs` never invokes npm; a cargo-only checkout still compiles (committed `dist/.gitkeep`, empty UI until built). Dev loop: Vite dev server with HMR proxying `/api/*` to `dev-server`, or single-server `vite build --watch` into the disk-served `ui/dist`. Sourcemaps exist in dev builds only.

### 4. Architecture

Per `02-architecture.md`:
`src/` uses the standard `pages/ components/ lib/ store/ types/styles/` cut.
Everything (mostly) user-visible is a component - `components/*` are DOM components (declarative templates, `mount(el, store)`), `components/canvas/*` are canvas components (pure `render(ctx, state, layout)`).
`lib/canvas` holds the shared drawing math, `lib/interact` the input state machines, and `lib/trace` is the single typed boundary through which all code reaches the frozen core. A typed store with explicit slices (trace, viewport, selection, uiPrefs, transient) replaces the ~68 globals; subscribers declare slice dependencies and a scheduler coalesces all rendering to one RAF tick.

### 5. Performance measured rules

From `03-performance-findings.md`, verified against the running viewer at 10.57MB and 84.6MB trace scale:

- Pixel-bounded rendering applies to EVERY draw primitive, strokes included
  (today `stroke()` is 76% of pan CPU at scale; one full render costs 860 ms).
- No render path may bypass the store scheduler; input handlers never call
  render functions (today wheel zoom runs 1:1 synchronous full renders).
- The overlay/pointer channel performs zero DOM writes and never reads layout
  after a write (today: one forced reflow per mousemove).
- Heap budget: parsed traces cost ~10x their raw bytes (measured 864 MB at the
  100 MB cap's scale); new code adds no per-event copies and keeps derived
  views windowed. No reload leak exists today; keep it that way.
- Derived-data caches keyed by store slices are a first-class store feature;
  visibility queries use binary-search windows over sorted invariants.
- Load pipeline may run the frozen core in a Web Worker unchanged (parse is
  currently a main-thread stall: 3.8 s / 12.2 s walls).

### 6. Frozen core policy

`decode.js`, `trace_parser.js`, `trace_analysis.js` and friends stay at the `ui/` root in their current form: canonical, Node-tested, consumed via CommonJS interop into page bundles (they do not tree-shake), typed via hand-written `.d.ts`. Trace-format compatibility semantics live there and are untouchable during the migration.

Analyzed and decided about the core:

- **WASM: rejected, for every file.** Parse already streams at ~33 MB/s overlapped with download and the 100 MB cap bounds the worst case; a WASM decoder would still have to marshal its output into the JS data the app consumes (recreating the dominant cost); `trace_analysis` hot functions run per render frame, where JS-to-WASM boundary crossings are worst; and a WASM decoder would make the Rust decoder responsible for old-trace leniency, an explicit non-goal today.
- **Data-model reshape (columnar storage, string interning, monomorphic records): justified but deferred.** It is the measured source of the ~10x heap and the megamorphic hot loops, but it rewrites exactly the code whose stability the "lose nothing" plan depends on. The migration manufactures its preconditions: `lib/trace` gives one seam to swap the data model behind, and the parity harness + NFR budgets give regression detection. Own ADR, triggered after the viewer.html parity gates pass.
- **Do now instead:** run the frozen core in a Web Worker in the new load pipeline (section 5) - zero core changes, removes the multi-second main-thread load stall.
- **Micro-fixes inside the core (known one-liners) are batched into the reshape ADR**, not applied piecemeal: each touch of a frozen file costs a stress cycle and erodes the freeze.
- **Test migration to Vitest does NOT require modernizing the core.** Vitest imports the CJS-guard core as-is; only test files are rewritten (section 7). Converting the core itself to modern ESM stays bundled with the reshape ADR.

### 7. Testing and verification

Vitest is the single test runner: new TS modules are Vitest-tested from the
start, and the existing ~24 plain-node `test_*.js` suites are MIGRATED to
Vitest. The migration is mechanical - harness assertions become
`describe`/`expect`, imports stay pointed at the untouched core via CJS interop

- and requires no change to the frozen core (Vitest consumes CJS as-is). It can
  land incrementally: both runners stay wired in CI until the last file moves.
  Wins: auto-discovery removes the standing footgun that a test never runs in CI
  unless hand-registered in `scripts/e2e-trace-tests.sh` (that script reduces to
  trace generation + `vitest run`); watch mode and coverage come free. The
  trace-file-parameterized integrity tests (stress workflow) switch from argv to
  an env var. CI: `tsc --noEmit`, `vitest run`, build check. Every migration
  slice passes a parity gate: the affected inventory rows walked against the
  running page (Playwright harness established during the index-page validation),
  plus the standard Rust-side test rules. NFR budgets are checked on the demo
  trace and a large trace.

### 8. Migration approach

The proposal is a-step one. First we create the new UI as progressively as below and we show a switch button to go the new one, but legacy remains default. Then we feel comfortable we can reverse the switch and allow users to keep using the legacy UI but new default is the migrated one. Finally we remove the legacy.

Regarding the migration, will be incremental, per surface, riskiest last: pipeline proof on `flamegraph.html` (smallest),then `index.html`, then `viewer.html` by slice (trace types -> store/viewport -> low-risk chrome -> panels one at a time -> lanes and pointer interactions). Old and new coexist during the migration via a static-copy list in the Vite config (legacy pages + core files into `dist/`) that shrinks to empty; every landed change deletes the inline code it replaces, no long-lived dual implementations. Done means: the three HTML shells contain no inline script beyond the module tag, the functional contract holds, and the NFR budgets hold.

### 9. UX direction

Audited for internal expert users (journeys inferred from the tool's own
diagnostic skills; three-lens judge panel over live-UI evidence; claims
re-verified before cataloging). Full findings: `04-ux-findings.md`. Decisions:

- **Priorities:** locate/share-a-moment (S2, S3) and keyboard ergonomics
  (K1-K3) lead; the audit's headline is that the tool computes the right
  answers but hides the surfaces that carry them.
- **Unified keyboard model (track A, adopted):** one vocabulary across all
  three pages - `/` search palette (tasks/spans/POIs), `n`/`p` POI stepping,
  `g` goto-time, `f` fit, `z` zoom-undo, WASD nav, `?` help everywhere;
  existing bindings unchanged. Interactive mock: `mocks/keyboard.html`.
- **View state becomes shareable:** URL carries viewport/selection/POI (the
  browser page already does this, #585); plus minimap overview strip and a
  status bar. These ride every concept.
- **Layout reorganization (track B, direction accepted; concept choice open):**
  three mocked concepts - unified timeline column / triage-first rail /
  conservative evolution (`mocks/concept-{1,2,3}.html`, hold `C` to compare
  with the current UI). The chosen concept amends the functional contract
  BEFORE the affected page's migration slice, so components are built once to
  the amended spec; pure visual polish trails as a later pass.
- UX changes are deliberate contract amendments and get the same parity-gate
  treatment as everything else (section 7).

### 10. Large traces: segment-windowed loading

Traces reach ~100 MB/min in S3; all-at-once loading cannot survive that (heap
is ~10x raw; the 100 MB cap and issues #421/#523 are the symptom). Decision:
two-tier pipeline - overview renders from S3 listing metadata + the EXISTING
server Parquet aggregates (`/api/tokio-stats`, `/api/flamegraph`) with zero raw
downloads; raw segments (independently parseable, verified) lazy-load for the
viewport window with +/-1-segment boundary prefetch and LRU eviction under a
resident budget. Frozen core untouched; byte-range requests rejected (gzipped
whole-file segments). The 100 MB open cap is replaced by the resident-window
budget; lifting it is the acceptance test. Full design: `02-architecture.md`
section 2.8, NFR N19.

## Consequences

- The repo gains a JS toolchain (vite, vitest, typescript as dev dependencies), two CI jobs, a release-pipeline build stage, and a documented Node requirement for UI contributors; end users are unaffected.
- JS supply-chain machinery starts existing: lockfile-pinned installs and a per-dependency justification norm (the tree starts at zero runtime deps).
- Rendering hot paths stay imperative canvas TypeScript; no vDOM ever enters the poll-render path.
- The inventories, constraints, architecture, and performance docs are the living specification; this ADR is their index and the record of the cleadecisions binding them together.
