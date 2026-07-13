// components/overlay/data.ts - the once-per-trace derivation the overlay's
// hover channel feeds to T22's assembler (T24). This is DATA PLUMBING, not
// hover logic: it gathers the arrays `assembleLaneHover` (T22) and the
// at-cursor readout consume, reusing the frozen-core builders. The hover
// LOGIC stays in components/canvas/lanes/hover.ts (T22) - we never reimplement
// it (scope fence).
//
// LaneData (T22) omits the GLOBAL injection-queue series and the active-task
// timeline (the lanes do not draw them); the tooltip/info readout need both,
// so we run the core builders once more here. This is a one-time O(events)
// cost at trace load (wired through store.derived over the `trace` slice), NOT
// a per-frame cost - the 03 F3 contract is about the per-hover path, which
// only does bounded lookups over these arrays.

import {
  attachCpuSamples,
  buildActiveTaskTimeline,
  buildSpanData,
  buildWorkerSpans,
} from "../../lib/trace/index.js";
import { deriveWorkerIds } from "../canvas/lanes/data.js";
import type {
  BlockInPlaceGap,
  ParsedTrace,
  TracingSpan,
  WorkerLane,
} from "../../types/trace.js";

/** Everything the hover channel needs beyond per-frame viewport/selection. */
export interface OverlayData {
  /** Worker ids in render order (same order the lanes stack, for y-mapping). */
  workerIds: number[];
  /** Reconstructed poll/park/active spans per worker, CPU samples attached. */
  workerSpans: Record<number, WorkerLane>;
  /** All completed spans, start-sorted (span detail + span-in-poll count). */
  allSpans: TracingSpan[];
  /** Per-worker local-queue series, sorted by t (G12 / local Q). */
  workerQueueSamples: Record<number, { t: number; local: number }[]>;
  /** Global injection-queue series, sorted by t (I6 / tooltip Global Q). */
  queueSamples: { t: number; global: number }[];
  /** Active-task-count timeline, sorted by t (I6 / tooltip Active Tasks). */
  activeTaskSamples: { t: number; count: number }[];
  blockInPlaceGaps: readonly BlockInPlaceGap[];
  hasCpuTime: boolean;
  hasSchedWait: boolean;
  hasTaskTracking: boolean;
}

/**
 * Derive the overlay's frame-invariant hover data for one parsed trace. Runs
 * the frozen core's buildWorkerSpans (+ attachCpuSamples), buildSpanData, and
 * buildActiveTaskTimeline. Call once per trace and cache (store.derived over
 * the `trace` slice) - never per frame.
 */
export function deriveOverlayData(trace: ParsedTrace): OverlayData {
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

  const spanData = buildSpanData(trace.customEvents);
  const timeline = buildActiveTaskTimeline(
    trace.taskSpawnTimes,
    trace.taskTerminateTimes,
  );

  return {
    workerIds,
    workerSpans,
    allSpans: spanData.allSpans,
    workerQueueSamples: spanResult.workerQueueSamples,
    queueSamples: spanResult.queueSamples,
    activeTaskSamples: timeline.activeTaskSamples,
    blockInPlaceGaps: trace.blockInPlaceGaps,
    hasCpuTime: trace.hasCpuTime,
    hasSchedWait: trace.hasSchedWait,
    hasTaskTracking: trace.hasTaskTracking,
  };
}
