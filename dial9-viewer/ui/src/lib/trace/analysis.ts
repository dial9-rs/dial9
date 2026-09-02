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
  activeTaskSeries,
  analyzeAllocations,
  attachCpuSamples,
  buildActiveTaskTimeline,
  buildFgData,
  buildFlamegraphTree,
  buildProcessCpuUsageSeries,
  buildRuntimeFilterData,
  buildSpanData,
  collectDescendants,
  computePollWakes,
  computeRuntimeGroups,
  computeSchedulingDelays,
  computeSpanLayout,
  filterPointsOfInterest,
  flattenFlamegraph,
  getTraceTimeRange,
  globalQueueSeries,
  hasCpuProfileSamples,
  selectSpanRenderSet,
  sumActiveTasksByCycle,
  sumGlobalQueueByCycle,
} from "../../../trace_analysis.js";

// buildWorkerSpans is DISPATCHED, not a pure re-export: the frozen core version
// retains event objects (incompatible with the viewer's columnar event store),
// so a columnar `trace.events` is routed to the columnar reimplementation. A
// fat `events` array keeps the byte-identical shared-core path.
import { buildWorkerSpans as frozenBuildWorkerSpans } from "../../../trace_analysis.js";
import { ColumnarEvents } from "./columnar-events.js";
import { buildWorkerSpansColumnar } from "./worker-spans-columnar.js";
import type { BlockInPlaceGap, WorkerSpansResult } from "../../../trace_analysis.js";
import type { TraceEvent } from "../../../trace_parser.js";

export function buildWorkerSpans(
  events: readonly TraceEvent[] | ColumnarEvents,
  workerIds: readonly number[],
  maxTs: number,
  blockInPlaceGaps?: readonly BlockInPlaceGap[]
): WorkerSpansResult {
  if (events instanceof ColumnarEvents) {
    return buildWorkerSpansColumnar(events, workerIds, maxTs, blockInPlaceGaps);
  }
  return frozenBuildWorkerSpans(events, workerIds, maxTs, blockInPlaceGaps);
}

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
