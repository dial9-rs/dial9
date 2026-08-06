// The once-per-trace derivation the overlay's hover channel feeds to the
// assembler. This is DATA PLUMBING, not hover logic: it gathers the arrays
// `assembleLaneHover` and the at-cursor readout consume. The hover LOGIC stays
// in components/canvas/lanes/hover.ts.
//
// LaneData omits the GLOBAL injection-queue series and the active-task timeline
// (the lanes do not draw them); the tooltip/info readout need both, so we run
// the core builders once more here. This is a one-time O(events) cost at trace
// load (wired through store.derived over the `trace` slice), NOT a per-frame
// cost.

import { buildActiveTaskTimeline } from "../../lib/trace/index.js";
import {
  deriveRuntimeGroups,
  deriveRuntimeMetrics,
  sharedSpanData,
  sharedWorkerSpans,
} from "../../lib/trace/derived.js";
import { metricsRuntimeNames } from "../../lib/trace/runtime-metrics-model.js";
import type {
  BlockInPlaceGap,
  ParsedTrace,
  RuntimeGroup,
  TracingSpan,
} from "../../types/trace.js";
import type { LaneSpans } from "../../lib/trace/columnar-worker-spans.js";
import type { ColumnarSpans } from "../../lib/trace/columnar-spans.js";

/** Everything the hover channel needs beyond per-frame viewport/selection. */
export interface OverlayData {
  /** Worker ids in render order (same order the lanes stack, for y-mapping). */
  workerIds: number[];
  /** Runtime groups in render order (for the fixed-height + header y-mapping). */
  runtimeGroups: RuntimeGroup[];
  /** Runtime names with a summary lane (so hit-test y-mapping matches the
   *  rendered stack, which appends a lane per such runtime). */
  metricsRuntimes: ReadonlySet<string>;
  /** Reconstructed poll/park/active spans per worker, CPU samples attached. */
  workerSpans: Record<number, LaneSpans>;
  /** All completed spans, start-sorted (span detail + span-in-poll count).
   * EMPTY on the columnar path - read `columnarSpans`. */
  allSpans: TracingSpan[];
  /** Columnar span store (main-thread path); hover span lookups dispatch on it. */
  columnarSpans?: ColumnarSpans | undefined;
  /** Per-worker local-queue series, sorted by t (local Q). */
  workerQueueSamples: Record<number, { t: number; local: number }[]>;
  /** Global injection-queue series, sorted by t (tooltip Global Q). */
  queueSamples: { t: number; global: number }[];
  /** Active-task-count timeline, sorted by t (tooltip Active Tasks). */
  activeTaskSamples: { t: number; count: number }[];
  blockInPlaceGaps: readonly BlockInPlaceGap[];
  hasCpuTime: boolean;
  hasSchedWait: boolean;
  hasTaskTracking: boolean;
  hasLocalQueueDepth: boolean;
}

/**
 * Derive the overlay's frame-invariant hover data for one parsed trace. Runs
 * the frozen core's buildWorkerSpans (+ attachCpuSamples), buildSpanData, and
 * buildActiveTaskTimeline. Call once per trace and cache (store.derived over
 * the `trace` slice) - never per frame.
 */
export function deriveOverlayData(trace: ParsedTrace): OverlayData {
  const runtimeGroups = deriveRuntimeGroups(trace);
  const workerIds = runtimeGroups.flatMap((g) => g.workerIds);
  const spanResult = sharedWorkerSpans(trace);
  const workerSpans = spanResult.workerSpans;

  // Runtimes with a metric series get a summary lane; the hit-test must account
  // for it (same derivation the renderer's data uses, so hover can never drift
  // from the drawn stack).
  const metricsRuntimes = metricsRuntimeNames(runtimeGroups, deriveRuntimeMetrics(trace));

  const spanData = sharedSpanData(trace);
  const timeline = buildActiveTaskTimeline(
    trace.taskSpawnTimes,
    trace.taskTerminateTimes,
  );

  return {
    workerIds,
    runtimeGroups,
    metricsRuntimes,
    workerSpans,
    allSpans: spanData.allSpans,
    columnarSpans: spanData.columnarSpans,
    workerQueueSamples: spanResult.workerQueueSamples,
    queueSamples: spanResult.queueSamples,
    activeTaskSamples: timeline.activeTaskSamples,
    blockInPlaceGaps: trace.blockInPlaceGaps,
    hasCpuTime: trace.hasCpuTime,
    hasSchedWait: trace.hasSchedWait,
    hasTaskTracking: trace.hasTaskTracking,
    hasLocalQueueDepth: trace.hasLocalQueueDepth,
  };
}
