# T08 HANDOFF - lib/canvas: shared drawing utilities

(Replaces the T07 HANDOFF inherited through the branch chain; T07's own
record lives at commit 63a18e8.)

## STATUS

DONE - all DoD checks pass (evidence below). No STOP-gate hit. Utilities
only: no components, no page wiring, no store changes, no frozen-core
.js edits, no page HTML, no Rust touched.

Note on ticket numbering: T07's HANDOFF listed "T08: store actions /
uiPrefs persistence" under REMAINING - that reflected its author's guess
at the ticket map, not this ticket. This T08 is lib/canvas per the
chunk-1 partition; store actions remain unowned by this branch.

## COMPLETED (commits on `ticket/T08-lib-canvas`, on top of 63a18e8)

- `db7cbc1` layout.ts + layout.test.ts: typed wrapper over the frozen
  panel_layout.js; LABEL_W pinned; PanelGeometry/LaneGeometry producers.
- `9e0254f` dpr.ts + dpr.test.ts: resize-only-on-geometry-change (F3);
  pure planBackingStore + createCanvasSizer binding.
- `a2c74c9` stroke.ts + stroke.test.ts: pixel-bounded, style-batched
  stroke building (the F1 fix direction as named contracts).
- `73c518e` downsample.ts, palette.ts, their tests, index.ts barrel.
- `ff69b43` coverage-gap tests -> 100% stmts/branch/funcs/lines on
  src/lib/canvas/**.
- (final commit): this HANDOFF.

## WHAT WAS BUILT (and the decisions inside it)

All under `dial9-viewer/ui/src/lib/canvas/`, tests colocated. The
barrel `index.ts` is the import surface for future canvas components;
only the modules behind it import the frozen core's drawing helpers.

### layout.ts (architecture 2.3 N4; ADR-0004 section 5)

- `timePanelLayout({pw, scrollbarW?, viewStart, viewEnd})` -> the frozen
  core's TimePanelLayout with labelW pinned to the exported LABEL_W=100.
  The wrapper deliberately has NO labelW parameter, so the historical
  200px-gutter bug cannot be rebuilt through this API.
- PURE, unlike viewer.html's timePanelLayout(panel, scrollbarW): callers
  pass measured widths in, so DOM reads can batch once per frame (F3).
  drawW <= 0 is surfaced, not masked (caller early-returns, legacy
  contract).
- `panelGeometry(...)` and `laneStackGeometry(workerIds, laneHeight)`
  produce the types/state.d.ts geometry shapes; layout.ts is their
  single producer as that file's comment mandates.

### dpr.ts (F3: backing stores resize only on geometry change)

- Pure decision: `planBackingStore(prev, next)` - exact compare of
  {cssWidth, cssHeight, dpr} against the last-APPLIED geometry. Never
  reads canvas.width back (integer truncation would make fractional
  sizes always look changed). Device size = round(css * dpr) - the
  legacy code let the canvas setter truncate, undersizing up to 1px.
- Thin binding: `createCanvasSizer(canvas).ensure(w, h, dpr?)` applies
  the plan; unchanged path performs ZERO canvas writes besides an
  absolute `setTransform(dpr,0,0,dpr,0,0)` (setTransform, not scale(),
  so repeated calls never compound; needed because the unchanged path no
  longer gets the implicit state reset of a width= write).
- The unchanged path does NOT clear the canvas (a resize used to clear
  as a side effect). Renderers must paint their full area - every legacy
  panel starts with a background fill, so this holds; documented on
  ensure().
- Typed against a minimal structural `DprCanvas<Ctx>`; the test file
  carries a compile-time proof that HTMLCanvasElement satisfies it, and
  drives the binding with a recording stub (write-count assertions).
- Context is fetched once and cached; getContext returning null throws
  (no silent no-op rendering).

### stroke.ts (F1: stroke() was 76% of pan CPU; ADR section 5 rule 1)

- `downsampleSeriesToColumns(points, {xOf, yOf, weightOf?, x0?, drawW})`
  -> at most one vertex per pixel column, BEFORE pathing. Representative
  policy mirrors the core's pixelDownsampleSpans: largest `weightOf`
  wins; default is last-in-column (step carry-out). Input must be sorted
  ascending by xOf - out-of-order THROWS instead of garbling the line
  (same contract the core documents but does not check). Off-view x
  clamps into the edge columns so the legacy lowerBound-1 pre-view
  sample keeps the line's entry height.
- `expandSteps(vertices)`: step-function knees inserted only between
  vertices >= 2 columns apart (sub-pixel steps skipped), bound
  <= 2 * ceil(drawW) - still O(width). The strict one-vertex-per-column
  guarantee (DoD) is series(); stepSeries() carries the documented 2x
  bound for zoomed-in step charts (the queue chart's shape at sparse
  sample counts cannot be represented under a 1x bound; both regimes
  stay O(width), never O(samples)).
- `makeStrokeBatcher()`: `.series()/.stepSeries()` (downsample + append
  as a subpath), `.polyline()` (escape hatch, caller owns the bound),
  `.tick(style, x, y1, y2)` (vertical markers deduped per pixel column,
  first wins - the open-ended-poll dashed-edge storm becomes O(width)),
  `.batches()` in first-use order.
- `drawStrokeBatches(ctx, batches, styleOf)`: per style ONE
  strokeStyle/lineWidth assignment, at most ONE setLineDash, ONE
  beginPath, ONE stroke - however many subpaths. Dash state restored to
  solid on return (no leakage into subsequent fills/strokes). ctx typed
  as structural StrokePathContext -> Node-testable with a recorder.

### downsample.ts / palette.ts

- downsample.ts: typed re-exports of the core fill primitives
  (pixelDownsampleSpans, makeBarCoalescer + BarCoalescer type,
  pixelCoverage) with their sorted-input contracts in the docs. Tests
  are wiring smoke checks only; core behavior stays covered by its own
  suites.
- palette.ts: re-exports pollHeatmapColor / pollHeatmapColorQuantized /
  flamegraphColor (exact core export names; the ticket's "pollColor" is
  NOT a core export - it is viewer.html inline lore, ported here):
  `pollColor(startNs, endNs)` = quantized duration color (issue #450
  bucketing contract, tested), `makeColorDimmer(factor=0.4)` = the
  memoized channel-multiply behind pollColorDim (throws on non-#rrggbb
  and factor outside [0,1]), `SPAN_COLORS` + `makeColorAssigner()` = the
  stable name->color assignment behind spanColor/ceColor (one assigner
  per namespace = the legacy two-maps behavior).

## INTEROP (the ticket asked this documented)

src/ modules import the frozen core with plain ESM named imports and a
root-relative specifier, e.g.
`import { makeTimePanelLayout } from "../../../panel_layout.js"`:

- tsc: matched by the T05 ambient wildcard declarations
  (`declare module "*/panel_layout.js"` etc.); clean under
  verbatimModuleSyntax because the value/type split is explicit
  (`import type` for types).
- Vitest: vite-node's CJS interop resolves the named exports from the
  browser-global + CJS-guard files as-is (verified empirically before
  building; 55 tests exercise the path).
- Vite build: unaffected today - nothing in the rollup input graph
  imports lib/canvas yet, so dist/ is unchanged. When a page ticket
  first pulls the barrel in, Vite's commonjs interop bundles the core
  file into the page chunk (the architecture 2.1 plan); the same import
  form is the one probe.ts already uses.
- createRequire stays a tests/core-only pattern (it would break browser
  bundles if used in src/).

## DoD EVIDENCE

All run in dial9-viewer/ui (npm ci done):

1. `npx tsc --noEmit`: clean (exit 0).
2. `npm run test`: 15 files, 201 tests, all pass (146 inherited + 55
   new: layout 9, dpr 14, stroke 22, downsample 3, palette 12 - counts
   after the coverage-gap commit).
3. DoD axes, each a named test:
   - layout invariant: "known case: pw=1200, sb=17 -> drawW=1083, axis
     spans [100, 1183]" + no-scrollbar case + gutter-cannot-diverge
     invariant (layout.test.ts).
   - stroke batcher pixel bound: seeded-random property test, 50 rounds
     of up to 10k points at drawW 1..2000, asserts vertices
     <= ceil(drawW) (stroke.test.ts "DoD: arbitrary random series...")
     plus the per-style variant through makeStrokeBatcher.
   - DPR no-op: planBackingStore identical-geometry case + binding test
     asserting ZERO canvas writes across repeated ensure() with a
     recording stub (dpr.test.ts "DoD: unchanged geometry...").
4. Coverage (`npx vitest run --coverage.enabled
   --coverage.include='src/lib/canvas/**' src/lib/canvas`):
   100% statements (152/152), 100% branches (75/75), 100% functions
   (26/26), 100% lines (141/141).
5. `npm run build`: dist listing unchanged - same 19 files as T07's
   recorded listing (dev-probe chunk + 4 legacy pages + 12 legacy
   scripts + 2 public assets). Local `vite build` deletes the tracked
   dist/.gitkeep; restored via git checkout before committing
   (pre-existing quirk, same as T06/T07).
6. Not run: cargo build/nextest/clippy (no .rs touched, no trace-format
   change; rust-embed embeds ui/dist, whose listing is unchanged - same
   justification as T06/T07 per the AGENTS.md JS-only rule). No
   test_*.js added, so no scripts/e2e-trace-tests.sh registration
   needed (vitest CI picks the new suites up via the src/**/*.test.ts
   include).

## REMAINING

None for T08. Intentionally left for later tickets:

- Canvas COMPONENTS (components/canvas/*) consuming this barrel; the
  renderer registry mapping slices -> panels (architecture 2.3).
- A binary-search "first visible index" helper (viewer.html's
  lowerBound/findFirstVisible lore) - callers of pixelDownsampleSpans
  need a startIdx; that helper reads sorted trace invariants and belongs
  next to the derived-data caches (F5 ticket / lib/trace), not in
  lib/canvas.
- Trailing right-edge extension of the queue step line
  (`ctx.lineTo(pw, lastY)` in legacy): a one-vertex caller concern via
  polyline(); left to the queue panel component.

## BLOCKERS

None.

## NOTES FOR THE INTEGRATOR / SUCCESSORS

- The stroke DoD bound is per series()/tick() call: series() emits
  <= ceil(drawW) vertices per subpath, tick() <= 1 subpath per column
  per style. polyline() is the unchecked escape hatch - reviewers should
  treat unexplained polyline() calls in components as a smell.
- drawStrokeBatches resets dash to solid but leaves
  strokeStyle/lineWidth at the last style's values - callers set their
  own state before further stroking, as all legacy code already does.
- createCanvasSizer assumes it is the ONLY writer of its canvas's size:
  one sizer per canvas, created once at component mount, not per frame
  (a fresh sizer forgets the applied geometry and resizes once).
- The unchanged-geometry path does not clear the canvas; components must
  fully repaint (all legacy panels do - background fill first).
- LOCKFILE: unchanged by this branch (T07's @vitest/coverage-v8 note
  still applies against T12's playwright addition).
