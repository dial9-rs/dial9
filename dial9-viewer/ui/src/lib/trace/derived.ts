// Once-per-trace derivations, computed once and shared by EVERY consumer.
//
// buildWorkerSpans and buildSpanData each scan the whole event stream and, on a
// 13M-event trace, each allocate on the order of a gigabyte. Seven load-frame
// consumers used to run them independently; these caches collapse that to one
// reconstruction per trace, keyed on ParsedTrace identity, so a reparse
// invalidates but a pan or a selection change never does.
//
// This lives in lib/trace rather than beside the lanes component because the
// spans/events/queue tracks, the minimap, the issues rail and task-detail all
// read it, and none of them render lanes.

import {
  EVENT_TYPES,
  attachCpuSamples,
  buildSpanData,
  buildWorkerSpans,
  computeRuntimeGroups,
} from "./index.js";
import type { SpanData } from "./index.js";
import { ColumnarEvents } from "./columnar-events.js";
import { buildWorkerSpansColumnarStore } from "./worker-spans-columnar.js";
import { buildSpanDataColumnar } from "./span-data-columnar.js";
import {
  fatLanes,
  type ColumnarWorkerSpans,
  type LaneWorkerSpans,
} from "./columnar-worker-spans.js";
import type { ParsedTrace, RuntimeGroup } from "../../types/trace.js";

/** The distinct worker ids in a trace, in runtime-group render order: scan
 *  non-queue/non-wake events for the worker set, then reorder to match the
 *  runtime groups so grouped lanes render in group order; single-runtime
 *  traces stay simple-sorted. */
export function deriveWorkerIds(trace: ParsedTrace): number[] {
  return deriveRuntimeGroups(trace).flatMap((g) => g.workerIds);
}

/**
 * The distinct worker ids on lifecycle events, sorted ascending. Queue samples
 * and wake events are skipped: their `workerId` is a queue/target, not a worker
 * that ran anything, so counting them would invent lanes.
 *
 * This is the SET; callers that need render order wrap it in
 * deriveRuntimeGroups. Consumers that only count workers (POI detectors, the
 * minimap ticks) take it directly.
 */
export function lifecycleWorkerIds(trace: ParsedTrace): number[] {
  const set = new Set<number>();
  for (const e of trace.events) {
    if (e.eventType === EVENT_TYPES.QueueSample || e.eventType === EVENT_TYPES.WakeEvent) {
      continue;
    }
    set.add(e.workerId);
  }
  return [...set].sort((a, b) => a - b);
}

/** The runtime groups in render order: the lifecycle worker set, grouped by
 *  runtime. A single-runtime trace yields one group; the lanes renderer draws
 *  headers only when there is more than one. */
export function deriveRuntimeGroups(trace: ParsedTrace): RuntimeGroup[] {
  return computeRuntimeGroups(lifecycleWorkerIds(trace), trace.runtimeWorkers);
}

/**
 * Derive the frame-invariant lane data for one parsed trace. Runs the frozen
 * core's buildWorkerSpans (which also extracts queue samples and wake
 * indices) + attachCpuSamples, and builds the span-id index from the span
 * data. Call once per trace and cache (store.derived over the `trace` slice).
 */
const workerSpansCache = new WeakMap<ParsedTrace, LaneWorkerSpans>();

/**
 * buildWorkerSpans computed ONCE per trace and shared across EVERY eager
 * load-frame consumer (lanes, spans/queue/events tracks, minimap POIs, issues
 * rail, task detail). Each used to rebuild it independently -> 7 full
 * O(events) span reconstructions on the single load frame, each ~1.9 GB for a
 * 13M-event trace and each retained in its own cache. Sharing one collapses
 * that to a single reconstruction.
 *
 * SAFETY: attachCpuSamples - the ONLY cross-consumer mutation - is applied
 * HERE, once, so the shared result carries cpu samples for the consumers that
 * render them (lanes, rail, overlay) while read-only consumers (minimap, queue,
 * spans/events tracks, task detail) simply ignore the extra poll fields. Its
 * writes are deterministic (sample.spawnLoc/inPoll on the shared trace.cpuSamples
 * take the same values every run), so attaching once reproduces the old
 * per-consumer result. buildWorkerSpans output is a per-worker dict keyed by the
 * event workers, independent of worker-id ORDER, and every caller derives the
 * same worker-id SET (or a subset it only reads back), so one result is correct
 * for all.
 */
export function sharedWorkerSpans(trace: ParsedTrace): LaneWorkerSpans {
  let r = workerSpansCache.get(trace);
  if (r === undefined) {
    const ev = trace.events;
    if (ev instanceof ColumnarEvents) {
      // Big-trace (main-thread) path: build the typed-array span store DIRECTLY
      // (no fat poll/park/active objects) and hand render/models columnar lane
      // VIEWS. The worker path (plain-array events) is untouched below.
      const built = buildWorkerSpansColumnarStore(
        ev, deriveWorkerIds(trace), trace.maxTs ?? 0, trace.blockInPlaceGaps,
      );
      if (trace.cpuSamples && trace.cpuSamples.length > 0) {
        // Frozen-core CpuSample[] bridges to the columnar store's sample type
        // (it reads only the common fields); same `as never` idiom as poi.ts.
        built.store.attachCpuSamples(trace.cpuSamples as never);
      }
      columnarStoreCache.set(trace, built.store);
      r = {
        workerSpans: built.store.workerLanes(),
        perWorker: {},
        queueSamples: built.queueSamples,
        workerQueueSamples: built.workerQueueSamples,
        maxLocalQueue: built.maxLocalQueue,
        wakesByTask: built.wakesByTask,
        wakesByWorker: built.wakesByWorker,
      };
    } else {
      const fat = buildWorkerSpans(
        trace.events,
        deriveWorkerIds(trace),
        trace.maxTs ?? 0,
        trace.blockInPlaceGaps,
      );
      // Guard cpuSamples: a real ParsedTrace always has the array, but some
      // consumers previously never touched it (minimap/queue/task-detail), so
      // their unit-test mock traces omit it.
      if (trace.cpuSamples && trace.cpuSamples.length > 0) {
        attachCpuSamples(trace.cpuSamples, fat.workerSpans);
      }
      r = fat;
    }
    workerSpansCache.set(trace, r);
  }
  return r;
}

/** The columnar span store for a trace, when it loaded on the main-thread
 * columnar path (undefined for the worker/fat path). Set by sharedWorkerSpans;
 * read by sharedSpanData to build span data over columns. */
const columnarStoreCache = new WeakMap<ParsedTrace, ColumnarWorkerSpans>();

/**
 * The columnar worker-span store for a trace, or undefined on the fat/worker
 * path. Ensures the shared reconstruction has run. POI/minimap detectors use it
 * to scan raw columns instead of materializing the flyweight lane views (which
 * the frozen filterPointsOfInterest / computeSchedulingDelays would do).
 */
export function columnarWorkerStoreFor(trace: ParsedTrace): ColumnarWorkerSpans | undefined {
  sharedWorkerSpans(trace);
  return columnarStoreCache.get(trace);
}

const spanDataCache = new WeakMap<ParsedTrace, SpanData>();

/**
 * buildSpanData (tracing-span reconstruction from customEvents) computed ONCE
 * per trace and shared across the lanes + spans-track + overlay derivations,
 * which previously each rebuilt it (~0.9 GB of span objects per copy for a
 * 13M-event trace). buildSpanData is read-only over its inputs and its output
 * spans are never mutated cross-consumer, so one result is safe to share.
 */
export function sharedSpanData(trace: ParsedTrace): SpanData {
  let r = spanDataCache.get(trace);
  if (r === undefined) {
    // Ensure the worker spans (and, on the columnar path, the store) are built.
    const workerSpans = sharedWorkerSpans(trace).workerSpans;
    const store = columnarStoreCache.get(trace);
    const spanEvents = trace.spanEvents;
    r =
      store && spanEvents
        ? buildSpanDataColumnar(spanEvents, store)
        : buildSpanData(trace.customEvents, fatLanes(workerSpans));
    spanDataCache.set(trace, r);
  }
  return r;
}
