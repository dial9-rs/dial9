# dial9-viewer UI: non-functional requirements and target architecture

Design companion to the feature inventories (`01..03-*.md`, the functional
contract), the constraints (`01-technical-constraints.md`, H*/S* references), and
the stack decision (`docs/adr/0003-viewer-ui-migration.md`: TypeScript + Vite,
no framework, npm allowed, dist built in CI before packaging).

Functional requirements are deliberately absent: the inventories ARE the
functional contract ("lose nothing"). This document covers only non-functional
requirements and the architecture of the replacement.

## 1. Non-functional requirements

Budgets are anchored in what the current code already achieves; the migration may
beat them but must not regress them.

### Performance

- **N1. Lane rendering scales by pixels, not events.** Per-frame lane work stays
  bounded by canvas width via pixel downsampling (one representative span per
  column) and run-length-coalesced fillRects, as today
  (`trace_analysis.js pixelDownsampleSpans`, `makeBarCoalescer`; inventory 02 G4).
  Traces with millions of polls remain interactive at full zoom-out.
- **N2. Interaction cadence.** Pointer-driven work is frame-coalesced: at most one
  full render per animation frame (`scheduleRenderAll` semantics, 02 A9) and
  crosshair/hover redraw on its own RAF channel touching only the overlay layer
  (02 A10). Mousemove must never trigger a synchronous full render.
- **N3. Streaming load.** Download and parse overlap (streaming decode) with
  progressive load-timing feedback, as shipped in `fe1b12c`; time-to-first-render
  on the demo trace does not regress.
- **N4. Rendering fidelity.** DPR-aware canvas scaling everywhere (02 A11);
  scrollbar-width compensation preserved (02 A12); the shared time-axis alignment
  invariant across timeline/lanes/panels holds pixel-exact (02 A13,
  `panel_layout.js`).
- **N5. Input caps.** The 100 MB open cap (`MAX_OPEN_BYTES`) and its disable+warn
  behavior remain (01 H4) until the segment-windowed pipeline (2.8) replaces it
  with the resident-window budget (N19).
- **N19. Scale by windowing, not refusal.** Traces at ~100 MB/min scale are
  navigable end to end: overview renders from listing metadata + server
  aggregates with zero raw downloads; raw segments load lazily for the
  viewport window with +/-1-segment boundary prefetch and LRU eviction under a
  resident budget (~100-150 MB raw, ~10x that parsed). Whole-range analyses
  on partial data are explicitly badged, never silently wrong. See 2.8.

### Size and dependencies

- **N6. Bundle budget.** Each page ships ONE bundle (new TS code + frozen core
  inlined via CommonJS interop); its size stays at or below the page's current
  total JS payload (inline script + module files), and minified comfortably
  under it. Tree-shaking (Rollup) applies to the new TS/ESM code; the CJS-guard
  frozen core enters wholesale and is not tree-shakeable. No runtime framework.
  Sourcemaps exist in dev builds only - never in the embedded release set. The
  embed is `ui/dist/` only (section 2.1), so the binary carries exactly the
  built servable assets - no test files, sources, or toolchain (01-technical-constraints.md S6).
- **N7. Dependency discipline.** Every new runtime npm dependency is justified in
  its PR (what it replaces, size, maintenance risk); lockfile pinned; dev
  dependencies limited to the toolchain (typescript, vite, vitest, test/browser
  tooling).

### Compatibility and operation

- **N8. Zero backend change.** Same `/api/*` endpoints, query parameters
  (repeatable `trace=`), credential headers (`x-dial9-aws-*`), and gunzip-client
  behavior (01 I4/I5/I7).
- **N9. Trace-format compatibility is inherited, not reimplemented.** The frozen
  core performs all decode/parse/analysis (H4); new code never parses wire bytes.
  Old traces with missing fields keep rendering (AGENTS.md rules).
- **N10. URL contract stability.** Every documented query parameter
  (`trace`, `svc`, `host`, `from`, `to`, `segs`, `bucket`, `prof`, zoom-state
  params on the flamegraph page) keeps its meaning; deep links into old traces
  keep working.
- **N11. Evergreen-browser baseline** (S5): es2022 output, no transpile-down, no
  polyfills.

### Correctness

- **N16. Strict typing is non-negotiable.** `tsc` runs with `strict: true` and
  `noUncheckedIndexedAccess`; no `any` escapes without an inline justification
  comment. The parsed-trace shape, store slices, and layout geometry are fully
  typed; exhaustive switches over event/panel kinds are compiler-enforced.
- **N17. Chrome cannot desynchronize from state.** DOM chrome renders
  declaratively from store state (see section 2.4); there is no code path that
  mutates chrome DOM outside a state-driven render. Removes the stale-DOM bug
  class (per R2 in ADR-0003).
- **N18. Dev-build assertions.** Invariants that the types cannot carry
  (time-axis alignment, downsampling column bounds, store-update ordering) are
  runtime-asserted in dev builds and compiled out of release bundles.

### Toolchain and process

- **N12. Install paths unchanged for users.** `cargo install --locked dial9` and
  binstall binaries ship built assets; Node is never required at install or run
  time (H1). `build.rs` never invokes npm.
- **N13. One test runner, no lost coverage.** Vitest is the single runner: new
  TS modules are Vitest-tested from the start, and the existing `test_*.js`
  suites are migrated to Vitest mechanically (assertions rewritten to
  `describe`/`expect`; imports keep consuming the untouched core via CJS
  interop; incremental, dual-runner CI until the last file moves).
  Auto-discovery removes the hand-registration footgun in
  `scripts/e2e-trace-tests.sh`, which reduces to trace generation +
  `vitest run`; the trace-file-parameterized integrity tests switch from argv
  to an env var. New code stays bare-`node` runnable (erasable-syntax TS +
  Node 24 type stripping), so the plain-node escape hatch is never foreclosed.
  CI adds `tsc --noEmit`, `vitest run`, and a build check.
- **N14. Dev loop parity or better.** Single-server mode (`vite build --watch`
  into the disk-served `ui/`) preserves the current loop exactly (H5); the Vite
  HMR dev server with an `/api` proxy is available on top.
- **N15. Migration safety.** Every landed step keeps all three pages fully
  functional (S3); parity is checked against the inventories (see section 5).

## 2. Target architecture

### 2.1 Directory layout

`ui/` is a self-contained, standard Vite multi-page project. The rust-embed
folder narrows from `ui/` to `ui/dist/`: the binary embeds exactly the built,
servable artifact set and nothing else (this also retires the current
indiscriminate embed of test files, 01-technical-constraints.md S6, with no exclusion lists to maintain).

```
dial9-viewer/ui/                       # the frontend project root, self-contained
  package.json, package-lock.json      # toolchain + deps
  node_modules/                        # gitignored
  vite.config.ts, tsconfig.json        # vitest config lives in vite.config.ts
  index.html, viewer.html, flamegraph.html   # Vite MPA entries (thin shells once migrated)
  public/                              # copied verbatim into dist/ (demo-trace.bin, flamegraph.css)
  src/
    pages/                             # one entry module per page: viewer/, browser/, flamegraph/
    components/                        # DOM components: toolbar, sidebar, popups, toasts, tooltip, help, tables
      canvas/                          # canvas components: lanes, crosshair, heatmap, panels/* (spans, custom-events, cpu, queue, task-detail)
    lib/
      canvas/                          # shared canvas utils: layout, DPR wrapper, downsampling, bar coalescing, palettes
      interact/                        # pointer, keyboard, wheel state machines
      trace/                           # typed wrappers around the frozen core
    store/                             # state slices: trace, viewport, selection, uiPrefs, transient
    types/                             # trace.d.ts, state.d.ts; .d.ts for the frozen core
    styles/                            # extracted CSS (imported by entries, bundled by Vite)
    **/*.test.ts                       # Vitest tests colocated with sources
  decode.js, trace_parser.js, trace_analysis.js, format.js, heatmap.js,
  prefix_detect.js, creds.js, panel_layout.js, flamegraph.js,
  flamegraph_export.js                 # frozen core at root, unchanged (test entry points, CI paths)
  test_*.js, test-traces/              # existing Node tests, untouched paths
  dist/                                # vite build output (gitignored, CI-built) = rust-embed folder
```

Rationale for the `src/` division: standard frontend vocabulary (the React-style
`components/ store/ lib/ types/ pages/` cut, minus framework-specific dirs),
carrying this app's real boundaries. Everything user-visible is a component; the
subfolder marks the render medium and therefore the contract: `components/*` are
DOM components (declarative templates, `mount(el, store) -> dispose`, 2.4) while
`components/canvas/*` are canvas components (pure `render(ctx, state, layout)`
functions, 2.3). `lib/` holds non-component logic: `lib/canvas` shared drawing
math, `lib/interact` input state machines (2.5), `lib/trace` typed core
wrappers. `store/` is the state model (2.2). `pages/` is Vite's native
multi-page concept: each HTML entry loads exactly one page module that composes
the rest.

The frozen core stays physically at the `ui/` root so its Node tests, CI paths
(`node dial9-viewer/ui/test_*.js`), and `file:line` anchors keep working. The
core files are browser-global scripts with CommonJS export guards (not ES
modules); Vite ingests them through its CommonJS interop and inlines them into
each page bundle. Hand-written `.d.ts` files in `src/types/` give them typed
signatures without touching them. They are NOT embedded (only `dist/` is);
during the migration, still-unmigrated pages that load them via `<script src>`
get them copied into `dist/` by a static-copy list in the Vite config that
shrinks to empty as pages migrate.

### 2.2 State model (kills the ~68 globals)

One small first-party typed store per page, with explicit slices:

- `trace`: the parsed trace object (produced by the frozen core; typed via
  `types/trace.d.ts`). Replaced wholesale on load/reparse, never mutated.
- `viewport`: `viewStart`/`viewEnd`, min/max bounds, zoom/pan ops with the
  existing clamps (100ns min span, bounds clamping).
- `selection`: selected task / focused span chain / pinned custom event /
  retained sidebar range / hovered waker - the cross-highlight state that today
  is scattered globals (02 G6-G8, I4-I5).
- `uiPrefs`: foldable-panel states, sidebar width, legend toggles; persisted to
  localStorage as today (02 O, P).
- `transient`: mouse position, keyboard-selection cursor, drag state; updates on
  the crosshair RAF channel, never triggers full renders.

Mechanics: plain object + `update(slice, patch)` + `subscribe(sliceSet, fn)`.
Subscribers declare which slices they depend on; the scheduler coalesces all
notifications into one RAF tick and runs each subscriber at most once per frame
(N2). No proxies, no reactivity library; the store is <200 lines and fully
Node-testable.

### 2.3 Render architecture

Two-layer canvas model, unchanged in spirit from today:

- **Content layers**: per-worker lane canvases and one canvas per analysis panel
  (spans, custom events, CPU, queue, task detail). Each is a canvas component
  (`components/canvas/`) exporting
  `render(ctx, state, layout)` - a pure function of store state and the shared
  layout. The renderer registry maps store slices to affected panels: viewport
  changes redraw all time panels; a selection change redraws only lanes + the
  panels that display selection highlights.
- **Overlay layer**: the fullscreen crosshair canvas (mouse line, keyboard
  cursor, event markers, selection overlays) redraws alone on the transient
  channel (02 I1-I5).
- **Layout**: `lib/canvas/layout.ts` wraps the frozen `panel_layout.js` invariant
  (LABEL_W gutter, drawW, scrollbar compensation) as the single source of
  geometry for every panel (N4).
- **Perf contracts as utilities**: pixel downsampling, bar coalescing, the poll
  heatmap palette, and the DPR canvas wrapper become typed utility modules with
  unit tests, so the tricks that make the viewer fast are named, tested contracts
  instead of inline lore (N1).

### 2.4 Chrome (DOM) components

DOM modules with a uniform shape: `mount(el, store) -> dispose`. Each owns its
subtree, subscribes to the slices it renders, and dispatches store actions on
user input. Rendering is declarative per R2/N17: each component is a pure
state-to-template function re-rendered on slice change via a micro templating
library (lit-html class: tagged templates with efficient DOM diffing, ~3-4 KB, no
component runtime, no vDOM tree). Imperative DOM mutation outside these render
functions is prohibited; event handlers only dispatch store actions. This removes
the stale-DOM/desync bug class that hand-written `element.textContent = ...`
updates carry, at near-zero runtime cost for chrome-sized DOM. The
inventory's chrome sections map 1:1 to modules: toolbar + POI navigation (02 C/D/E),
sidebar + tabs (02 P/Q/R), foldable panels (02 O), toasts (02 U), tooltip (02 V),
help overlay (02 T), file loading + drop zone (02 B), and on the browser page the
search controls, heatmap chrome, raw table, and creds panel (01 C/D/F/G/H).

### 2.5 Interaction layer

`lib/interact/pointer.ts` centralizes the lane pointer state machine that today is
interleaved event handlers (02 H): plain drag = pan, Shift+drag = region select,
Alt+drag = zoom select, click = task/span select, with the existing thresholds
(3px drag intent, 100ns minimum). Keyboard (02 H9/H11/H12, T) and wheel (02 H5)
get sibling modules. Interaction modules translate raw events into store actions
and overlay-channel updates only - they never render.

### 2.6 Typing strategy

The single highest-leverage correctness move: `types/trace.d.ts` describing
the parsed-trace shape the frozen core produces (workers, polls, spans, custom
events, CPU/heap samples, sched events, queue series, block-in-place gaps). Every
component/interact module consumes typed state; `tsc --noEmit` enforces it in
CI with `strict: true` + `noUncheckedIndexedAccess` (N16). ADR-0002 semantics
(unknowable block-in-place gaps) are encoded in the types as explicit nullability
rather than tribal knowledge, and kind-discriminated unions make event/panel
switches compiler-exhaustive.

### 2.7 lib/trace: the typed boundary around the frozen core

`lib/trace` is the only place in `src/` permitted to import the frozen core
(`decode.js`, `trace_parser.js`, `trace_analysis.js`) directly; everything else
consumes its typed API via the barrel. This keeps exactly one seam between the
untyped CJS core and the strict-TS application. Contents are a mix of typed
wrappers over the core and extractions of trace-adjacent logic that is inline
HTML script today:

- `load.ts` - load orchestration: repeatable `trace=` components, parallel
  fetch + gunzip + concat, streaming parse with progress callbacks, file-drop
  and demo paths (wraps `fetchTraces`/`parseTrace`; 02 B, 01 I4).
- `reparse.ts` - time-range windowed re-parse for Set/Clear Range (02 E3/E4).
- `query.ts` - per-interaction read helpers: poll-at-timestamp binary search,
  span ancestor walk (1024 cycle guard), enclosing spans, task lookups
  (02 G13/G14, I).
- `analysis.ts` - typed facade over `trace_analysis.js`: flamegraph tree
  builds (CPU/heap/idle), blocking-call analysis, task lifecycle.
- `keys.ts` - S3 key parsing extracted from inline `parseKey` (01 I2), with
  the `layout: 'known' | 'unknown'` discriminant (section 4 defect fix).
- `title.ts` - `traceTitleParams` header metadata shared by all three pages
  (01 I3).
- `index.ts` - barrel; the only import surface the rest of `src/` sees.

Where a drawing-math utility's implementation lives in the frozen core (e.g.
`pixelDownsampleSpans`), `lib/canvas` re-exports it typed through this same
boundary rather than importing the core itself.

### 2.8 Large-trace data pipeline (segment-windowed loading)

Traces reach ~100 MB/min in S3; parsed heap is ~10x raw (03 F4), so
all-at-once loading tops out around one minute of data (hence the 100 MB cap,
#421, #523). Replacement: a two-tier model built on three verified properties
of the existing system:

- Segments are independently parseable: a mid-stream `TRC\0` header resets
  schemas/string pools/timestamp base (`trace_parser.js:252`), so every S3
  object is a self-contained decode unit.
- The segment is the atomic transfer unit: already time-partitioned in S3
  (`.../{date}/{HHMM}/.../{epoch}-{index}.bin.gz`), whole-file gzipped, and
  `/api/object` has no Range support - so byte-range loading is rejected, and
  per-segment time extents come free from listing metadata (the index heatmap
  already does this).
- A server-side aggregation tier already exists: `ingest/aggregate` folds raw
  segments into Parquet (per-poll rollups, stack dictionaries, CPU samples
  partitioned by service/date/host), queried by `/api/tokio-stats` and
  `/api/flamegraph` with progressive refinement (`refine=1`).

**Tier 1 - overview (no raw bytes).** The minimap, cold-open density strip,
and any zoomed-out view beyond the resident window render from cheap sources:
S3 listing metadata (segment extents + sizes) and the Parquet aggregate
endpoints. Opening a multi-hour trace shows a navigable overview immediately,
before any raw segment downloads.

**Tier 2 - detail (lazy segment window).** The store gains a `segments` slice:
a map from segment key to state (`listed -> fetching -> parsed -> evicted`)
plus per-segment time extent. The viewport drives it:

- Need set = segments overlapping `[viewStart, viewEnd]`; fetch + parse the
  missing ones (in the Web Worker, section 1 N-notes), render as they arrive.
- Prefetch = +/-1 segment beyond both edges (the boundary preload), fetched at
  idle priority so panning feels instant crossing a boundary.
- Eviction = LRU by distance from the viewport once the resident budget is
  exceeded; evicted segments fall back to tier-1 rendering. Parsed-window
  invariants (min/max ts, worker set) are retained so lanes/axes stay stable.
- Analyses that are inherently whole-range (flamegraph over a selection wider
  than the resident window, cross-segment task follows) either scope to the
  resident window with an explicit "partial data" badge or delegate to the
  server aggregate endpoints - never silently wrong.

**Budget (N19).** Resident raw bytes capped at a store constant (~100-150 MB
raw, ~10x that parsed) - a window limit, not a load-time rejection. The 100 MB
open cap and its "refuse to open" warnings (01 H4) are replaced by it.

Placement: the segment-window machinery is `lib/trace/segments.ts` + the
`segments` store slice; `load.ts` becomes its bootstrap (list, fetch initial
window); tier-1 sources go through `lib/trace/aggregates.ts` (typed client for
the existing endpoints). The frozen core is untouched - it already parses
segments independently; the new code only decides WHICH segments to hand it.

**Hard edges (each is a production bug if skipped):**

- Stale-fetch cancellation: viewport jumps abort in-flight fetches and make
  worker parse jobs discardable (AbortController); without it, queued stale
  work recreates the #523 hang inside the new design.
- Boundary-truncated polls: a poll spanning a window edge (PollStart resident,
  PollEnd evicted/unfetched) renders as explicitly truncated - the existing
  open-ended-poll marker (features/02 G5) extended to window edges - never as
  a long-poll false positive. ADR-0002 gap detection assumes park/unpark
  continuity and must treat window edges like trace edges.
- Two-level cache: eviction drops PARSED data (the 10x cost) but retains raw
  gzipped bytes under a separate larger budget; re-entering a window re-parses
  (~33 MB/s) instead of re-downloading - kills S3 GET churn on pan-back.
- Aggregate coverage: the Parquet pipeline can lag or miss hosts (the API
  exposes `Coverage`); tier 1 degrades to listing-metadata density and never
  blocks on aggregates.
- Feedback + hysteresis: per-segment fetch/parse progress surfaces in the
  status bar; eviction triggers below the hard budget since GC lags eviction.

Non-goal: live/appending traces (follow mode) - explicitly out of scope for
the migration.

Sequencing: the store/segment machinery lands with the viewer.html migration
slices (it IS the new load pipeline); tier-1 aggregate rendering can land
earlier as it only touches new code. Full lazy windows are the enabling
mechanism for ever lifting the 100 MB cap - lifting it is the acceptance test.

## 3. Build, test, and CI pipeline

- `npm run build`: `vite build` (Rollup) in standard multi-page mode: the three
  HTML files at the project root are the entries; Vite processes them, bundles
  their page modules (`src/pages/*`), and emits HTML + assets + `public/`
  copies into `ui/dist/` (es2022, minified). Because Vite rewrites asset
  references in the emitted HTML, default content-hashed filenames are fine.
  The frozen core is inlined from the `ui/` root via CommonJS interop (it does
  not tree-shake; new TS code does). Sourcemaps only in dev builds, never in
  the embedded release set.
- `npm run dev` (primary loop): Vite dev server with HMR, `server.proxy` routing
  `/api/*` to the running `dev-server`. `npm run dev:embedded` (fallback,
  single-server): `vite build --watch` into `ui/dist` with `dev-server` serving
  `ui/dist` from disk (`with_dev_ui_dir` repointed), preserving the current
  edit-refresh loop.
- `npm test`: Vitest, the single runner (node environment by default, jsdom
  where DOM is needed; watch mode + coverage locally) - covers new TS modules
  and, per N13, the migrated legacy suites; the frozen core sources stay
  untouched under test. During the incremental test migration, not-yet-moved
  `test_*.js` files keep running via `scripts/e2e-trace-tests.sh`.
- CI additions: `tsc --noEmit`, `vitest run`, and a `vite build` job proving
  dist compiles (N13).
- Release (`release.yml` + crates.io publish): `npm ci && npm run build` BEFORE
  `cargo publish` / binary builds, so packaged crates and binstall archives carry
  `ui/dist` (N12). Lockfile-pinned (N7).

## 4. Known-defect placement (structural fixes, not spot patches)

- Raw-table dead column sort (01 G8): the raw results table becomes a small
  sortable-table chrome component; sorting is a component capability, so the
  affordance and the behavior can no longer diverge.
- `parseKey` silent mislabel (01 validation Finding 1): the typed key-parse
  result gains an explicit `layout: known | unknown` discriminant; UI renders
  unknown layouts as raw keys instead of shifted columns.
- dial9 bucket-filter lockout (01 validation Finding 2): the picker filter
  becomes a config-provided predicate (server config or query param) with the
  current behavior as default, so dev/demo backends are reachable.
- Time-only heatmap axis ambiguity (01 validation Finding 3): the shared axis
  utility renders date + time when the visible span crosses a day boundary.

## 5. Migration order and parity gates

Each step lands independently; all three pages work after every merge (N15).

1. **Scaffolding**: package.json + toolchain configs under `ui/`, the `src/`
   skeleton, switching the rust-embed folder (and `with_dev_ui_dir`) from `ui/`
   to `ui/dist/`, the migration static-copy list (legacy pages + core files
   into dist), gitignores (`node_modules/`, `dist/*`) with a committed
   `dist/.gitkeep` so cargo-only checkouts still compile (empty UI until
   `npm run build`; H1), CI jobs, release-pipeline build stage, dev-loop
   scripts.
2. **flamegraph.html** (smallest surface, 03): proves the whole pipeline end to
   end - typed entry, bundle, release artifacts - while touching the least code.
3. **index.html** (01): browser page; heatmap logic is already a clean module,
   so this is mostly chrome extraction plus the G8/bucket-filter fixes.
4. **viewer.html** (02), by slice, riskiest last: trace types -> store +
   viewport -> low-risk chrome (toasts, help, tooltip) -> panels one at a time
   (CPU, queue, custom events, spans, task detail) -> lanes + pointer/keyboard
   interactions.
5. Each extraction PR deletes the inline code it replaces (no long-lived dual
   implementations) and passes the parity gate: the relevant inventory rows
   walked against the running page (Playwright harness from the 01 validation,
   extended per surface), plus the existing Node test suite and stress run per
   repo rules.

Definition of done for the migration: the three HTML shells contain no inline
script beyond the module tag, every inventory row is either observably intact or
consciously retired with sign-off, and the NFR budgets in section 1 hold on the
demo trace and a large real trace.
