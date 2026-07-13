// The once-per-trace lane derivation: frame-invariant data derived once and
// cached, never recomputed per pan/selection frame. Produces worker ids,
// reconstructed spans, per-worker queue series, wake indices, and the span-id
// index the highlight reads.
//
// This is the DERIVED input to render.ts. The mount wires it through the
// store's derived() cache keyed by the `trace` slice, so it recomputes only
// when a new trace loads/reparses - not on viewport or selection change.

import {
  EVENT_TYPES,
  attachCpuSamples,
  buildSpanData,
  buildWorkerSpans,
  computeRuntimeGroups,
  spansById as buildSpanByIdSingle,
} from "../../../lib/trace/index.js";
import type {
  ParsedTrace,
  TracingSpan,
  WorkerLane,
  WorkerWake,
} from "../../../types/trace.js";

/** Everything render.ts needs that is invariant across viewport/selection. */
export interface LaneData {
  /** Worker ids in render order (runtime-group order, single-runtime = sorted). */
  workerIds: number[];
  /** Reconstructed poll/park/active spans per worker, CPU samples attached. */
  workerSpans: Record<number, WorkerLane>;
  /** Per-worker local-queue samples, sorted by t. */
  workerQueueSamples: Record<number, { t: number; local: number }[]>;
  /** Wake events indexed by target worker. */
  wakesByWorker: Record<number, WorkerWake[]>;
  /** span id -> every span instance sharing it (highlight lookup, no scan). */
  spansById: Map<string, TracingSpan[]>;
  /** All completed spans, start-sorted (containing-span lookup). */
  allSpans: TracingSpan[];
  /** span id -> a single span, for the ancestor walk. */
  spanByIdSingle: Map<string, TracingSpan>;
  hasCpuTime: boolean;
  hasSchedWait: boolean;
  hasTaskTracking: boolean;
}

/** The distinct worker ids in a trace, in runtime-group render order: scan
 *  non-queue/non-wake events for the worker set, then reorder to match the
 *  runtime groups so grouped lanes render in group order; single-runtime
 *  traces stay simple-sorted. */
export function deriveWorkerIds(trace: ParsedTrace): number[] {
  const set = new Set<number>();
  for (const e of trace.events) {
    if (e.eventType === EVENT_TYPES.QueueSample || e.eventType === EVENT_TYPES.WakeEvent) {
      continue;
    }
    set.add(e.workerId);
  }
  const sorted = [...set].sort((a, b) => a - b);
  const groups = computeRuntimeGroups(sorted, trace.runtimeWorkers);
  return groups.flatMap((g) => g.workerIds);
}

/**
 * Derive the frame-invariant lane data for one parsed trace. Runs the frozen
 * core's buildWorkerSpans (which also extracts queue samples and wake
 * indices) + attachCpuSamples, and builds the span-id index from the span
 * data. Call once per trace and cache (store.derived over the `trace` slice).
 */
export function deriveLaneData(trace: ParsedTrace): LaneData {
  const workerIds = deriveWorkerIds(trace);
  const maxTs = trace.maxTs ?? 0;
  const spanResult = buildWorkerSpans(
    trace.events,
    workerIds,
    maxTs,
    trace.blockInPlaceGaps,
  );
  const workerSpans = spanResult.workerSpans;
  if (trace.cpuSamples.length > 0) {
    attachCpuSamples(trace.cpuSamples, workerSpans);
  }

  // Span-id index: id -> every instance (recycled ids highlight every
  // instance, but O(1) lookup instead of a full allSpans scan).
  const spanData = buildSpanData(trace.customEvents);
  const spansById = new Map<string, TracingSpan[]>();
  for (const s of spanData.allSpans) {
    let bucket = spansById.get(s.spanId);
    if (!bucket) {
      bucket = [];
      spansById.set(s.spanId, bucket);
    }
    bucket.push(s);
  }

  return {
    workerIds,
    workerSpans,
    workerQueueSamples: spanResult.workerQueueSamples,
    wakesByWorker: spanResult.wakesByWorker,
    spansById,
    allSpans: spanData.allSpans,
    spanByIdSingle: buildSpanByIdSingle(spanData.allSpans),
    hasCpuTime: trace.hasCpuTime,
    hasSchedWait: trace.hasSchedWait,
    hasTaskTracking: trace.hasTaskTracking,
  };
}
