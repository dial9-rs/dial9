// lib/trace/analysis.ts - typed facade over the frozen trace_analysis.js
// (T09; architecture 2.7): flamegraph tree builds (CPU via
// buildFlamegraphTree/buildFgData, heap via analyzeAllocations, idle/sched
// through the same flamegraph builders over sched samples), blocking-call
// analysis (computeSchedulingDelays/computePollWakes - the sched-events
// panel and per-poll blocking detection), task lifecycle
// (buildActiveTaskTimeline over spawn/terminate times), plus worker-span
// reconstruction, runtime groups, tracing spans and points of interest.
//
// Pure re-exports: the implementations are frozen (ADR-0004 section 6) and
// typed by src/types/trace_analysis.d.ts; this module exists so nothing
// outside lib/trace imports the core file. Deliberately NOT here because
// lib/canvas owns the drawing math re-exports (architecture 2.3/T08):
// pixelDownsampleSpans, pixelCoverage, makeBarCoalescer, pollHeatmapColor,
// pollHeatmapColorQuantized, flamegraphColor. `enclosingSpans` is exported
// by ./query.ts with the other per-interaction reads.
//
// computeSpanLayout sits on the fence (it maps spans to pixel buckets);
// it lives here for now and may move behind lib/canvas when the span-panel
// component ticket lands.

// MUST be the first dependency: trace_analysis.js's factory reads the
// TraceParser browser global in bundled entries (see core-globals.ts).
import "./core-globals.js";

export {
  analyzeAllocations,
  attachCpuSamples,
  buildActiveTaskTimeline,
  buildFgData,
  buildFlamegraphTree,
  buildProcessCpuUsageSeries,
  buildRuntimeFilterData,
  buildSpanData,
  buildWorkerSpans,
  collectDescendants,
  computePollWakes,
  computeRuntimeGroups,
  computeSchedulingDelays,
  computeSpanLayout,
  filterPointsOfInterest,
  flattenFlamegraph,
  getTraceTimeRange,
  hasCpuProfileSamples,
  selectSpanRenderSet,
} from "../../../trace_analysis.js";

export type {
  ActiveSpan,
  AllocationAnalysis,
  AllocationSite,
  FlamegraphNode,
  FlamegraphSampleInput,
  FlatFlamegraphNode,
  ParkSpan,
  PointOfInterest,
  PointOfInterestType,
  PollSpan,
  ProcessCpuUsageInterval,
  ProcessCpuUsageSample,
  RuntimeFilterData,
  RuntimeGroup,
  SchedDelay,
  SpanData,
  SpanLayoutBucket,
  SpanSegment,
  TaskWake,
  TracingSpan,
  UnmatchedSpan,
  WorkerLane,
  WorkerSpansResult,
  WorkerWake,
} from "../../../trace_analysis.js";
