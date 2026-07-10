# T06 HANDOFF - App-level trace + state types

(Replaces the T10 HANDOFF inherited through the merged branch chain;
T10's own record lives on `ticket/T10-vitest-first-batch` / its worktree.)

## STATUS

DONE - all DoD checks pass (evidence below). No gate hit: the spec's
options had a clearly preferable reading everywhere (choices documented
below). No frozen-core .js, no test_*.js, no page HTML, no Rust touched.

## COMPLETED (commits on `ticket/T06-app-level-types`, on top of merge 9b25022)

- `f1d0418`: src/types/trace.d.ts - app-level trace vocabulary.
- `d5511e7`: src/types/state.d.ts - store slices + panels + geometry.
- `9bb41ef`: src/types/exhaustive.test.ts - exhaustive-switch DoD test.
- (final commit): this HANDOFF.

## WHAT THE TYPES ARE (and the decisions inside them)

### Form: importable module .d.ts (vs T05's ambient wildcards)

T05's declarations are ambient (`declare module "*/x.js"`) because they
describe real frozen-core .js files reached by relative import. trace.d.ts
and state.d.ts have NO backing .js module, so a wildcard/ambient form
would invite runtime imports of a nonexistent file. They are instead
plain module .d.ts files containing TYPES ONLY (no `export const` - a
value declared there would be a lie at runtime). Consumers write
`import type { ... } from "../types/trace.js"` (or `./state.js`);
TS resolves the `.js` specifier to the `.d.ts`, `import type` is erased
by Vite, and `verbatimModuleSyntax` makes any accidental VALUE import a
TS1484 compile error, so a runtime import of the missing module cannot
ship. This composes with T05: trace.d.ts re-exports the core shapes from
the wildcard modules (type-only), giving src/ one import surface.

### trace.d.ts

- Re-exports the T05 core shapes (ParsedTrace, TraceEvent, CpuSample,
  CustomTraceEvent, BlockInPlaceGap, PollSpan/ParkSpan/ActiveSpan/
  WorkerLane, TracingSpan/SpanData, FlamegraphNode, allocation shapes,
  ...) - referenced, never redeclared.
- `TimeRange {startNs, endNs}` shared primitive (sidebar range, segment
  extents, re-parse windows).
- `RuntimeEvent` kind-discriminated union + `RuntimeEventKind`: one
  variant per trace_parser.js EVENT_TYPES entry (poll-start, poll-end,
  worker-park, worker-unpark, queue-sample, wake), string `kind`
  discriminant, each variant carrying ONLY the fields processFrame
  actually populates for that type (verified against trace_parser.js
  processFrame, lines ~735-850). The flat-TraceEvent -> union refinement
  is lib/trace's job (architecture 2.7, later ticket); this defines the
  vocabulary.
- ADR-0002 encoded explicitly, never silently optional: `tid` on the
  park/unpark variants is a REQUIRED `number | undefined` - constructors
  must write `tid: undefined` for old traces that predate the field;
  omitting it is a compile error. Doc comments carry the gap semantics
  (detection skips events without tid; no fabricated attribution).

### state.d.ts

- Panel unions from the real vocabulary: `FoldablePanelKind =
  "spans" | "events" | "cpu" | "queue"` (the literal data-panel-key
  values in viewer.html; also the localStorage key suffixes, features
  02 O4) and `PanelKind = FoldablePanelKind | "task-detail"`
  (task-detail is a panel but not foldable, features 02 N).
- Store slices per architecture 2.2 + 2.8, aggregated as `StoreState`
  with `StoreSliceName = keyof StoreState`:
  - `TraceSlice { trace: ParsedTrace | null }` - replaced wholesale,
    never mutated; derived analyses are computed, not stored.
  - `ViewportSlice { viewStart, viewEnd, minTs, maxTs }` - clamps
    (100ns min span, bounds clamping) documented as store-action
    behavior for T07/T08, not encoded as types.
  - `SelectionSlice { selectedTaskId, spanFocus (SpanFocus: clicked
    span + ancestor chain, 02 G7), focusedSpanId (span-panel subtree
    filter - deliberately distinct from spanFocus; both exist as
    separate globals today: selectedSpanId/selectedSpanIds vs
    focusedSpanId), pinnedEvent (PinnedCustomEvent incl. detailEvent
    replacing selectedEventRef), sidebarRange (TimeRange | null,
    replacing sidebarSelStart/End), hoveredWakerTaskId }` - all
    independently clearable, hence all explicitly nullable.
  - `UiPrefsSlice { panelCollapsed: Record<FoldablePanelKind, boolean>,
    sidebarWidth, selectedSpanNames/selectedEventNames (legend chip
    toggles, 02 J9/K5) }`.
  - `TransientSlice { mouseNs, hoverEventTs, drag: DragState | null,
    keyboardSelection: KeyboardSelection | null }` with `DragKind =
    "pan" | "region-select" | "zoom-select"` (2.5 vocabulary; the
    legacy shift/alt naming maps to region/zoom) and keyboard selection
    mirroring the two selection modes.
  - `SegmentsSlice`: `ReadonlyMap<string, SegmentEntry>` keyed by S3
    object key; `SegmentLifecycle = "listed" | "fetching" | "parsed" |
    "evicted"` exactly per 2.8, entry carries extent (TimeRange, trace
    ns, known from listing before any fetch) + sizeBytes. Payload
    refinement (raw-byte cache, abort handles) is left to the segments
    implementation ticket - lifecycle vocabulary only.
- Layout geometry wrapping panel_layout outputs: re-exports
  `TimePanelLayout`, adds `LaneGeometry` (worker lane row) and
  `PanelGeometry { kind, time: TimePanelLayout, height, dpr }` - the
  `render(ctx, state, layout)` vocabulary; lib/canvas/layout.ts (2.3)
  is the intended single producer.

### Test placement (ticket said "your call, document it")

Colocated at `src/types/exhaustive.test.ts`. Reason: the vitest include
(vite.config.ts) is `["tests/core/**/*.test.ts", "src/**/*.test.ts"]` -
a `tests/types/` directory would NOT be picked up without editing
vite.config.ts, and tests/core/ is reserved for migrated legacy suites.
Colocation is also the architecture 2.1 convention (`**/*.test.ts`
colocated with sources). The test doubles as the use-site type anchor
for the .d.ts bodies (skipLibCheck: true means they are otherwise only
checked at use sites, same rationale as T05's probe.ts).

## DoD EVIDENCE

All run in dial9-viewer/ui after `npm ci`:

1. `npx tsc --noEmit`: clean (exit 0).
2. Exhaustive-switch DoD: temporarily adding
   `| { kind: "task-spawn"; timestamp: number }` to RuntimeEvent made
   `npx tsc --noEmit` fail (exit 2) with:
   - `src/types/exhaustive.test.ts(59,13): error TS2322: Type '"task-spawn"' is not assignable to type 'never'.` (eventTypeId switch)
   - `src/types/exhaustive.test.ts(85,13): error TS2322: Type '{ kind: "task-spawn"; timestamp: number; }' is not assignable to type 'never'.` (describeEvent switch)
   Variant reverted; clean run confirmed again. The demonstration is
   recorded in the test file header; the same never-default pattern
   guards PanelKind and SegmentLifecycle.
3. `npm run test`: 9 files, 128 tests, all pass (8 migrated legacy
   suites + the new exhaustive test).
4. `npm run build`: dist listing byte-identical to the pre-change
   baseline (19 files; captured before the first edit, diffed after -
   no new artifacts; the .d.ts/.test.ts files are not in any build
   input).
5. Not run: cargo/nextest/clippy (no .rs touched, no trace-format
   change; rust-embed embeds only ui/dist, whose listing is unchanged -
   per ticket "you likely need no cargo at all" and the AGENTS.md
   JS-only rule). No new test_*.js at the ui/ root, so no
   scripts/e2e-trace-tests.sh registration is needed (vitest CI runs
   the new test via the include glob).

## REMAINING

None for T06. Intentionally left for later tickets:

- The flat-TraceEvent -> RuntimeEvent refinement function (lib/trace,
  architecture 2.7 ticket).
- Store implementation: update/subscribe, actions, clamps, localStorage
  persistence (T07/T08).
- Segment machinery payloads (raw-byte cache, AbortController handles)
  and lib/trace/segments.ts (2.8 implementation ticket).
- lib/canvas/layout.ts producing LaneGeometry/PanelGeometry (2.3).

## BLOCKERS

None.

## NOTES FOR SUCCESSORS

- Keep trace.d.ts/state.d.ts free of value declarations; if a typed
  CONSTANT is ever needed (e.g. MIN_VIEW_SPAN_NS = 100), it belongs in
  a real .ts module (store or lib), not in these .d.ts files.
- When adding a RuntimeEvent/PanelKind/SegmentLifecycle variant, expect
  compile failures in every never-default switch - that is the designed
  workflow; fix each site rather than loosening the default.
