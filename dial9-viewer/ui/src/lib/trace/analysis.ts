// Typed facade over the frozen trace_analysis.js: flamegraph tree builds,
// blocking-call analysis, task-lifecycle timelines, worker-span
// reconstruction, runtime groups, tracing spans, and points of interest.
// Pure re-exports (the implementations are frozen) so nothing outside
// lib/trace imports the core file directly. Drawing-math re-exports live in
// lib/canvas instead; `enclosingSpans` is re-exported by ./query.ts.

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
