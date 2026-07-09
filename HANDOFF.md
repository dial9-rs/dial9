# T05 HANDOFF - Type declarations for the frozen core

## STATUS

DONE - all DoD checks pass (evidence below). No core .js file, test_*.js,
or HTML page was modified. No gate was hit after the initial wrong-base
worktree issue (resolved by the orchestrator: switched in place to
be2c009, the T02 tip).

## COMPLETED (commits on `ticket/T05-core-type-declarations`, based on be2c009)

- `674ace2`: src/types/{decode,trace_parser,format}.d.ts
- `15971f3`: src/types/trace_analysis.d.ts
- `f9b09ec`: src/types/{heatmap,prefix_detect,creds,panel_layout}.d.ts
- `261217b`: src/types/{flamegraph,flamegraph_export}.d.ts + src/types/probe.ts
- (final commit): this HANDOFF.

All 10 core modules from the ticket's Owns list are declared. The
`.gitkeep` in src/types/ was left in place (harmless; T02 artifact).

## DECLARATION FORM (chosen + tradeoffs)

Ambient wildcard module declarations, one .d.ts per core file in
src/types/: `declare module "*/trace_parser.js" { ... }` etc.
(the form suggested in the ticket's implementation notes).

Why it works here:

- Consumers import the core via RELATIVE paths (e.g.
  `import { parseTrace } from "../../trace_parser.js"` from
  src/lib/trace). TS wildcard ambient modules match relative specifiers
  by suffix (same mechanism as Vite's `*.css` declarations), and with
  `allowJs` off the real .js file cannot shadow the ambient declaration.
  So ONE declaration types every import depth (src/pages, src/lib/trace,
  src/components) without path-specific duplicates. Relative names are
  not legal in non-wildcard ambient module declarations, so this is also
  the only ambient form available.
- Cross-module type reuse works by importing one wildcard module from
  inside another's block (e.g. trace_analysis.d.ts does
  `import type { CpuSample } from "*/trace_parser.js"`), so ParsedTrace /
  CpuSample / FlamegraphNode exist in exactly one place.

Tradeoffs T06+ should know:

- The pattern matches ANY specifier ending in `/decode.js` etc.,
  anywhere. If someone ever adds an unrelated file with a colliding
  basename (a second `format.js` in src/, say), it would silently get
  these types. Mitigation: core basenames are distinctive; keep it so.
- A BARE specifier without a slash (`import "trace_parser.js"`) does NOT
  match. All realistic imports are relative (contain a slash), so this
  is theoretical.
- `skipLibCheck: true` (T02 tsconfig) means the .d.ts bodies are not
  self-checked; correctness is enforced at use sites, which is exactly
  what probe.ts pins down (see negative check below).
- The declarations describe the CJS-interop ESM view (named exports from
  `module.exports`). Vite's interop provides these at runtime; `node
  test_*.js` CJS consumers are unaffected (no core file changed).

## API-SHAPE NOTES (exactness decisions)

- ADR-0002 encoded explicitly:
  - `TraceEvent.tid?: number | undefined` (park/unpark only; absent on
    old traces -- gap detection skips them).
  - `BlockInPlaceGap {workerId, fromTid, toTid, startNs, endNs}` on
    `ParsedTrace.blockInPlaceGaps`, doc-commented as unknowable
    attribution; `OFF_WORKER_WORKER_ID` sentinel documented on
    `CpuSample.workerId`.
  - `buildWorkerSpans(events, workerIds, maxTs, blockInPlaceGaps?)` --
    4th param optional (flamegraph.html calls with 3 args, viewer.html
    with 4); gap-crossing active spans are DISCARDED, so `ActiveSpan`
    has a non-nullable `ratio` and the doc comment states the absence
    semantics.
  - `ParkSpan.schedWait?: number` -- the synthetic trace-end park has no
    closing unpark, hence no schedWait (verified in buildWorkerSpans).
- `PollSpan.openEnded?: boolean`, `cpuSamples?/schedSamples?` and
  `CpuSample.spawnLoc?/inPoll?` are optional because attachCpuSamples
  assigns them after the fact.
- `ParsedTrace.minTs/maxTs/recordMinTs/recordMaxTs/clockOffsetNs/
  filterStartTime/filterEndTime` are `number | null` (matches
  finalizeParse and the trace-loading skill).
- Trace-format BC (AGENTS.md): later-added wire fields are optional in
  the types (`tid`, `CpuSample.cpu: number | null` for OptionalVarint).
- `unknown` used only where data is genuinely dynamic, each with an
  inline justification comment: `formatFieldValue(value: unknown)` and
  `escapeXml(s: unknown)` (both stringify anything). Schema-driven event
  fields use the closed `DecodedFieldValue` union from decode.d.ts
  instead of `unknown`/`any`. Zero `any` anywhere.
- Generics preserve caller element types where the core is shape-
  polymorphic: `pixelDownsampleSpans<S>`, `computeSpanLayout<S>`,
  `filterCpuSamples<S>`, `groupByHost<S>/tileSegments<S>/
  segmentsOverlapping<S>` (index.html rows keep their `key` field).
- `makeTimePanelLayout(pw, labelW, scrollbarW: number | undefined, ...)`
  -- viewer.html's wrapper forwards an optional param positionally.
- `parseTrace` is overloaded: buffer -> `Promise<ParsedTrace>`; string
  path (Node-only) -> `AsyncIterable<ParsedTrace>`.
- `Dial9Creds` typed as the module export; the `window.Dial9Creds`
  userscript global is deliberately NOT declared (typed src/ code should
  import the module; noted for T06 if a global is ever wanted).

## DoD EVIDENCE

1. `npx tsc --noEmit` (in dial9-viewer/ui, after `npm ci`): PASSES with
   probe.ts importing every declared API from the real relative paths
   (`../../<core>.js`) and using each in a type-checked position.
2. Negative check (declarations are live, not silently `any`): a temp
   file with `const x: number = parseTrace(new Uint8Array())` and
   `makeTimePanelLayout(1, 2, 3)` produced
   `error TS2322: Type 'Promise<ParsedTrace>' is not assignable to type 'number'`
   and `error TS2554: Expected 5 arguments, but got 3.`; removed, clean
   run confirmed again.
3. Probe does not ship: `npm run build` output lists only the dev-probe
   placeholder chunk + the 18 static-copied legacy files;
   `find dist -name "*probe*"` shows no T05 probe artifact.
4. `cargo build -p dial9-viewer` run to confirm rust-embed (ui/dist) is
   unaffected by the new src/types files (AGENTS.md JS-only rule; no .js
   touched, no trace-format change, so no JS tests are relevant and
   nextest/stress are not required).

### Module -> verified call site (declaration reviewed against each)

| Module | Call site verified | Shape checked |
|---|---|---|
| decode.js | trace_parser.js:9-18 (getTraceDecoder), 1160-1184 (parse loop), 981 (`dec.schemas.get(typeId)?.units`) | ctor(Uint8Array), decodeHeader, nextFrame loop, position/byteLength, streaming snapshot/restore/setBuffer/rewindToStart, schemas map (no direct HTML call site: decode.js is consumed only via trace_parser.js; Node consumer test_stream_parse.js) |
| trace_parser.js | viewer.html:1693-1882; flamegraph.html:517-534 | canStreamDecode / fetchTraceStream(url,{headers}) / fetchTracesStream(list,{headers}) / parseTraceStream(stream,opts) / fetchTraces(list,{headers}) / parseTrace(buffer); viewer.html:1016,5475,6078 formatFrame/symbolizeChain/deduplicateSamples |
| trace_analysis.js | viewer.html:1969-2402 (getTraceTimeRange, computeRuntimeGroups, buildWorkerSpans(4-arg), buildActiveTaskTimeline, computeSchedulingDelays, buildProcessCpuUsageSeries, buildSpanData, filterPointsOfInterest with taskInstrumented), 2994/3123/4606 (pixelDownsampleSpans, makeBarCoalescer, pixelCoverage), 1084 (pollHeatmapColorQuantized), 3421/3441/4049/4476 (selectSpanRenderSet, computeSpanLayout, enclosingSpans, computePollWakes); flamegraph.html:559-561 (buildWorkerSpans 3-arg, attachCpuSamples) | all mirrored in probe.ts probeAnalysis() |
| format.js | viewer.html:2178 (formatHumanDuration), 3370 (formatFieldValue); flamegraph.html:347 | number in -> string out; unknown value param |
| heatmap.js | index.html:1225-1237 (segment construction + groupByHost), 1274 (densityColor), 1311-1317 (tileSegments/segmentGaps/bootTransitions), 1351 (accumulateDensity), 1425 | generic segment flows keep the `key` field |
| prefix_detect.js | index.html:900 (isDateLayer(prefixes)) | string[] -> boolean; lastSegment string -> string |
| creds.js | index.html:443-471 (headers/get/has, apply path); tokio_stats.html:385, flamegraph.html:390/537 (headers(), has()) | headers() spreadable Record<string,string>; set/parse/check result shapes |
| panel_layout.js | viewer.html:2834/2868 (makeTimePanelLayout(clientWidth, LABEL_W, scrollbarW?, viewStart, viewEnd)) | undefined-able scrollbarW accepted |
| flamegraph.js | flamegraph.html:336/412/570/619-620 (createFlamegraph, setTreeDirect(toFgTree(...), count), filterCpuSamples, onZoomChange cb); viewer.html:6702/6750/6967/7041 (setData with exportTitle/exportFormatValue/formatCount, heap pseudo-samples with weight/allocWeight) | toFgTree's minimal {name,count,self,children:Map} node satisfies FlamegraphNode |
| flamegraph_export.js | flamegraph.js:252-270 (treeToInteractiveSvg(panels,{title,formatValue}), treeToFolded, filenameStem) -- consumed only via flamegraph.js by design, no direct HTML call site | panel {label,tree} array, null-tree panels allowed |

## REMAINING

None for T05. Out-of-scope items intentionally left:

- `types/trace.d.ts` app-level shapes (T06 refines on top of these).
- `flamegraph_api.js` and `url_state.js` are NOT in T05's module list
  (post-freeze page modules per vite.config.ts), so no declarations.
- Window global declarations (Dial9Creds/PanelLayout/etc.) -- legacy
  pages use globals, typed src/ should import; T06 call if needed.

## BLOCKERS

None.

## UNRELATED OBSERVATIONS (scope fence: not touched)

- trace_analysis.js `buildWorkerSpans` has a defensive fallback
  `openPollMeta[w] || { taskId: 0, spawnLocId: 0, spawnLoc: null }` whose
  `spawnLocId: 0` (a number) contradicts the field's `string|null` type
  everywhere else. Believed unreachable (openPoll/openPollMeta are set
  together); declared `string | null`. Worth a micro-fix in the deferred
  core-reshape batch (ADR-0004 section 6), not now.
- The dial9-trace-analysis skill documents buildWorkerSpans' wake index
  maps as part of its return but omits `perWorker`; the code returns it
  and the declaration includes it.
