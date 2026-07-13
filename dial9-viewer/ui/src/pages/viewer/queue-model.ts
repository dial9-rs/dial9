// The Queue depth track's pure model: no store, no DOM, no canvas - everything
// the queue-track controller needs to derive its series, scale them, and answer
// the drag-select.
//
// Issue #282: a 0-valued global-queue area rendered as a 1px stroke fused to
// the axis, indistinguishable from "no data" (invisible at zero). `queueScaleY`
// reserves ZERO_BASELINE_PX below the lowest data so a 0-valued series maps to
// a VISIBLE flat line a fixed distance above the axis. The underlying numbers
// are unchanged - only the y-mapping does.
//
// The three series (global injection queue, max per-worker local queue,
// active-task count) share ONE explicit zero baseline even though the
// active-task line keeps its own right-axis magnitude scale, so all three read
// against the same floor.

import type { ParsedTrace, TimeRange } from "../../types/trace.js";
import type { StoreState } from "../../types/state.js";
import { buildActiveTaskTimeline, buildWorkerSpans } from "../../lib/trace/index.js";
import { deriveWorkerIds } from "../../components/canvas/lanes/data.js";

// ── Windowing descriptor ─────────────────────────────────────────────────

/**
 * The resident-window state the queue renderer must surface so a truncated /
 * partial window is never painted as complete. Both fields resolve to the
 * "complete" value for a whole-trace load. Mirrors the CPU track's `CpuWindow`.
 */
export interface QueueWindow {
  truncatedAt: "start" | "end" | "both" | null;
  oversized: boolean;
}

/** The "complete window" descriptor (whole-trace load, no windowing). */
export const COMPLETE_QUEUE_WINDOW: QueueWindow = {
  truncatedAt: null,
  oversized: false,
};

// ── Series data (derived once per trace) ─────────────────────────────────

/** One entry of the merged, sorted local-queue timeline. */
export interface MergedLocalSample {
  t: number;
  /** Worker the sample belongs to. */
  w: number;
  local: number;
}

/**
 * Everything the queue track needs for one render pass + the drag-select,
 * lifted out of the store so the render logic stays Node-testable. Built once
 * per loaded trace by `computeQueueData` (store.derived over the `trace`
 * slice), never per frame.
 */
export interface QueueData {
  /** Worker ids in render order (same set the lanes/overlay use). */
  workerIds: readonly number[];
  /** Global injection-queue series, sorted by t. */
  queueSamples: readonly { t: number; global: number }[];
  /**
   * All per-worker local-queue samples merged into one t-sorted timeline, so
   * the render steps it once instead of re-merging per frame (max-local line).
   */
  mergedLocalSamples: readonly MergedLocalSample[];
  /** Active-task-count timeline, sorted by t. */
  activeTaskSamples: readonly { t: number; count: number }[];
  /** task id -> first-poll time, the spawn proxy. */
  taskFirstPoll: ReadonlyMap<number, number>;
  /** task id -> spawn-location id (null when unknown). */
  taskSpawnLocs: ReadonlyMap<number, string | null>;
  /** spawn-location id -> human-readable location. */
  spawnLocations: ReadonlyMap<string, string>;
  /** True when the trace carries the active-task timeline. */
  hasTaskTracking: boolean;
}

/** The empty (no-trace) queue data. */
export const EMPTY_QUEUE_DATA: QueueData = {
  workerIds: [],
  queueSamples: [],
  mergedLocalSamples: [],
  activeTaskSamples: [],
  taskFirstPoll: new Map(),
  taskSpawnLocs: new Map(),
  spawnLocations: new Map(),
  hasTaskTracking: false,
};

/**
 * Derive the queue track's frame-invariant data for one parsed trace. Runs
 * buildWorkerSpans (global + per-worker queue series) and
 * buildActiveTaskTimeline (active-task counts + taskFirstPoll). Call once per
 * trace and cache (store.derived over the `trace` slice) - never per frame.
 */
export function computeQueueData(trace: ParsedTrace | null): QueueData {
  if (trace === null || trace.maxTs === null) return EMPTY_QUEUE_DATA;
  const workerIds = deriveWorkerIds(trace);
  const spanResult = buildWorkerSpans(
    trace.events,
    workerIds,
    trace.maxTs,
    trace.blockInPlaceGaps,
  );
  const timeline = buildActiveTaskTimeline(
    trace.taskSpawnTimes,
    trace.taskTerminateTimes,
  );

  // Merge every worker's local-queue series into one t-sorted timeline, built
  // ONCE here so the per-frame render never re-merges.
  const merged: MergedLocalSample[] = [];
  for (const w of workerIds) {
    const samples = spanResult.workerQueueSamples[w];
    if (!samples) continue;
    for (const s of samples) merged.push({ t: s.t, w, local: s.local });
  }
  merged.sort((a, b) => a.t - b.t);

  return {
    workerIds,
    queueSamples: spanResult.queueSamples,
    mergedLocalSamples: merged,
    activeTaskSamples: timeline.activeTaskSamples,
    taskFirstPoll: timeline.taskFirstPoll,
    taskSpawnLocs: trace.taskSpawnLocs,
    spawnLocations: trace.spawnLocations,
    hasTaskTracking: trace.hasTaskTracking,
  };
}

/**
 * Derive the window descriptor from the store: any segment stuck "oversized"
 * makes the queue view unavoidably partial. The whole-trace path has an empty
 * segments slice, so this resolves to complete; `truncatedAt` stays null while
 * the renderer consumes the descriptor regardless. Mirrors cpu.ts
 * `deriveCpuWindow`.
 */
export function deriveQueueWindow(state: StoreState): QueueWindow {
  for (const entry of state.segments.segments.values()) {
    if (entry.state === "oversized") {
      return { truncatedAt: null, oversized: true };
    }
  }
  return COMPLETE_QUEUE_WINDOW;
}

// ── The scale function ───────────────────────────────────────────────────

/**
 * Pixels reserved between the chart's true bottom (the axis) and the y a value
 * of 0 maps to, so a 0-valued series renders as a VISIBLE flat line this far
 * above the axis instead of a stroke fused to it. Small enough to cost almost
 * no vertical range, large enough to read as a distinct line.
 */
export const ZERO_BASELINE_PX = 3;

/**
 * Map a queue-series value in [0, max] to a y coordinate inside the chart band
 * [chartTop, chartTop + chartH]. The bottom ZERO_BASELINE_PX is reserved so
 * value 0 lands a visible distance ABOVE the axis (the explicit zero baseline):
 * `queueScaleY(0, ...)` is strictly less than `chartTop + chartH`. `max` <= 0
 * pins everything to that baseline. The result is clamped into the band.
 *
 * This is the single scale every series (global, max-local, active-task) runs
 * through, so a 0-valued global renders a visible flat line.
 */
export function queueScaleY(
  value: number,
  max: number,
  chartTop: number,
  chartH: number,
  baselinePx: number = ZERO_BASELINE_PX,
): number {
  const reserve = Math.min(Math.max(0, baselinePx), chartH);
  const usableH = chartH - reserve;
  const baselineY = chartTop + chartH - reserve;
  if (!(max > 0) || usableH <= 0) return baselineY;
  const norm = value <= 0 ? 0 : value >= max ? 1 : value / max;
  return baselineY - norm * usableH;
}

/** The y a value of 0 maps to (the visible zero baseline). */
export function queueBaselineY(
  chartTop: number,
  chartH: number,
  baselinePx: number = ZERO_BASELINE_PX,
): number {
  return chartTop + chartH - Math.min(Math.max(0, baselinePx), chartH);
}

// ── Per-frame render model (bucketing) ───────────────────────────────────

/** The active-task overlay: a step line on its own right-axis magnitude. */
export interface QueueActiveTaskModel {
  /** Step-line vertices in draw-area x (px) + raw count (y via queueScaleY). */
  points: readonly { x: number; count: number }[];
  /** Count at viewStart (the step line's left seed). */
  startCount: number;
  /** Right-axis magnitude, >= 1. */
  maxTasks: number;
}

/**
 * The bucketed queue render model for one frame. `global[i]` / `local[i]` are
 * the plotted (carry-forward-applied) step values at pixel column i. `maxQ` is
 * the shared global+local magnitude (max(1, ...)).
 */
export interface QueueRenderModel {
  numBuckets: number;
  /** Plotted global value per pixel column (carry-forward step). */
  global: readonly number[];
  /** Plotted max-local value per pixel column (carry-forward step). */
  local: readonly number[];
  /** Shared magnitude for the global + local series (>= 1). */
  maxQ: number;
  /** The active-task overlay, or null when the trace has no task timeline. */
  activeTask: QueueActiveTaskModel | null;
  /** True when any series has data in view (else the "no data" path). */
  hasData: boolean;
}

/** Inputs for `buildQueueRenderModel` (all primitives - Node-testable). */
export interface QueueRenderInputs {
  data: QueueData;
  viewStart: number;
  viewEnd: number;
  /** Draw-area width in CSS px (drawW; the canvas already omits LABEL_W). */
  drawW: number;
}

/** Binary search: index of the first entry with `.t` >= target (lowerBound). */
function lowerBoundT<T extends { t: number }>(arr: readonly T[], target: number): number {
  let lo = 0;
  let hi = arr.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (arr[mid]!.t < target) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * Bucket the global + local + active-task series into `drawW` pixel columns for
 * one view window: global takes the max sample per pixel bucket; local steps
 * the merged timeline tracking each worker's current depth and takes the max
 * across workers per bucket; both carry the last value forward across empty
 * buckets. `maxQ` is max(1, peak global, peak local). The active-task overlay
 * reproduces maxTasks + countAtViewStart on its own scale.
 */
export function buildQueueRenderModel(inputs: QueueRenderInputs): QueueRenderModel {
  const { data, viewStart, viewEnd, drawW } = inputs;
  const numBuckets = Math.max(0, Math.ceil(drawW));
  const global = new Array<number>(numBuckets).fill(0);
  const local = new Array<number>(numBuckets).fill(0);
  const viewDur = viewEnd - viewStart;

  if (numBuckets === 0 || viewDur <= 0) {
    return { numBuckets, global, local, maxQ: 1, activeTask: null, hasData: false };
  }

  let hasData = false;

  // ── Global queue: max sample per pixel bucket ───────────────────────────
  const bucketGlobal = new Array<number>(numBuckets).fill(0);
  const bucketHasData = new Array<boolean>(numBuckets).fill(false);
  {
    const startIdx = Math.max(0, lowerBoundT(data.queueSamples, viewStart) - 1);
    for (let i = startIdx; i < data.queueSamples.length; i++) {
      const s = data.queueSamples[i]!;
      if (s.t > viewEnd) break;
      if (s.t < viewStart) continue;
      const bi = Math.floor(((s.t - viewStart) / viewDur) * (numBuckets - 1));
      if (bi < 0 || bi >= numBuckets) continue;
      bucketHasData[bi] = true;
      if (s.global > bucketGlobal[bi]!) bucketGlobal[bi] = s.global;
      hasData = true;
    }
  }

  // ── Local queue: single pass over the merged timeline ───────────────────
  const merged = data.mergedLocalSamples;
  const workerState = new Map<number, number>();
  for (const w of data.workerIds) workerState.set(w, 0);
  // Seed each worker from the sample just before viewStart.
  const mergeStart = Math.max(0, lowerBoundT(merged, viewStart) - 1);
  for (let i = mergeStart; i < merged.length; i++) {
    const s = merged[i]!;
    if (s.t >= viewStart) break;
    workerState.set(s.w, s.local);
  }
  const bucketLocal = new Array<number>(numBuckets).fill(0);
  let sampleIdx = Math.max(mergeStart, lowerBoundT(merged, viewStart));
  for (let bi = 0; bi < numBuckets; bi++) {
    const bucketEnd = viewStart + ((bi + 1) / numBuckets) * viewDur;
    while (sampleIdx < merged.length && merged[sampleIdx]!.t < bucketEnd) {
      const s = merged[sampleIdx++]!;
      workerState.set(s.w, s.local);
      bucketHasData[bi] = true;
      hasData = true;
    }
    let max = 0;
    for (const w of data.workerIds) {
      const v = workerState.get(w) ?? 0;
      if (v > max) max = v;
    }
    bucketLocal[bi] = max;
  }

  // maxQ across visible buckets.
  let maxQ = 1;
  for (let i = 0; i < numBuckets; i++) {
    if (bucketGlobal[i]! > maxQ) maxQ = bucketGlobal[i]!;
    if (bucketLocal[i]! > maxQ) maxQ = bucketLocal[i]!;
  }

  // Carry-forward so global[i]/local[i] are the plotted step value at pixel i:
  // update only on hasData buckets, else carry the last value. Start at 0 (the
  // baseline) until the first data bucket.
  let lastG = 0;
  let lastL = 0;
  for (let i = 0; i < numBuckets; i++) {
    if (bucketHasData[i]) {
      lastG = bucketGlobal[i]!;
      lastL = bucketLocal[i]!;
    }
    global[i] = lastG;
    local[i] = lastL;
  }

  const activeTask = buildActiveTaskModel(data, viewStart, viewEnd, viewDur, drawW);
  if (activeTask !== null) hasData = true;

  return { numBuckets, global, local, maxQ, activeTask, hasData };
}

/**
 * The active-task step-line model: maxTasks = max(1, peak count in view, count
 * at viewStart); the step line is seeded with countAtViewStart and one vertex
 * per in-view sample, at draw-area x. Null when the trace has no active-task
 * timeline.
 */
function buildActiveTaskModel(
  data: QueueData,
  viewStart: number,
  viewEnd: number,
  viewDur: number,
  drawW: number,
): QueueActiveTaskModel | null {
  const samples = data.activeTaskSamples;
  if (samples.length === 0) return null;

  let maxTasks = 1;
  for (const s of samples) {
    if (s.t >= viewStart && s.t <= viewEnd && s.count > maxTasks) maxTasks = s.count;
  }
  let startCount = 0;
  for (const s of samples) {
    if (s.t > viewStart) break;
    startCount = s.count;
  }
  if (startCount > maxTasks) maxTasks = startCount;

  const points: { x: number; count: number }[] = [];
  for (const s of samples) {
    if (s.t < viewStart) continue;
    if (s.t > viewEnd) break;
    const x = ((s.t - viewStart) / viewDur) * drawW;
    points.push({ x, count: s.count });
  }
  return { points, startCount, maxTasks };
}

// ── Drag-select: tasks spawned in a time range ────────────────────────────

/** One spawn-location group of tasks first polled inside the range. */
export interface SpawnedTaskGroup {
  /** Spawn location (or "(unknown)"). */
  loc: string;
  /** Tasks in this group, in taskFirstPoll iteration order. */
  tasks: readonly { taskId: number; firstPoll: number }[];
}

/**
 * The drag-select result: tasks whose FIRST POLL falls in `range`, grouped by
 * spawn location and sorted by count desc. This is the derivation half - the
 * track dispatches the range to `selection.spawnedTasksRange`; the inspector
 * renders this. Kept here (a pure port, minus the DOM) so the inspector reuses
 * the same logic instead of reimplementing it.
 */
export interface SpawnedTasksResult {
  range: TimeRange;
  /** Total tasks found in range. */
  total: number;
  /** Groups sorted by task count desc. */
  groups: readonly SpawnedTaskGroup[];
}

/**
 * Compute the spawned-task groups for a time range, or null when no task was
 * first polled in range. Uses `taskFirstPoll` as the spawn proxy (NOT
 * taskSpawnTimes). Bounds are inclusive on both ends.
 */
export function computeSpawnedTasks(
  data: QueueData,
  range: TimeRange,
): SpawnedTasksResult | null {
  const { startNs, endNs } = range;
  const groups = new Map<string, { taskId: number; firstPoll: number }[]>();
  let total = 0;
  for (const [taskId, t] of data.taskFirstPoll) {
    if (t < startNs || t > endNs) continue;
    const locId = data.taskSpawnLocs.get(taskId);
    const raw = locId != null ? data.spawnLocations.get(locId) : null;
    // An empty/missing location groups as unknown.
    const loc = raw || "(unknown)";
    let bucket = groups.get(loc);
    if (bucket === undefined) {
      bucket = [];
      groups.set(loc, bucket);
    }
    bucket.push({ taskId, firstPoll: t });
    total++;
  }
  if (total === 0) return null;
  const sorted = [...groups.entries()]
    .sort((a, b) => b[1].length - a[1].length)
    .map(([loc, tasks]) => ({ loc, tasks }));
  return { range, total, groups: sorted };
}

// ── Legend ─────────────────────────────────────────────────────────────────

/** One queue-legend entry: a swatch encoding + its meaning (matches the draw). */
export interface QueueLegendEntry {
  /** Swatch fill (CSS color). */
  swatch: string;
  label: string;
  /** Shape hint so the swatch reads as an area/line, matching the render. */
  shape: "area" | "line";
}

/**
 * The queue track legend: every series the track draws is explained, with
 * swatch encodings that MATCH the in-track rendering. Ordered global -> local
 * -> active-task, the draw order.
 */
export const QUEUE_LEGEND: readonly QueueLegendEntry[] = [
  { swatch: "#4fc3f7", label: "Global queue", shape: "area" },
  { swatch: "#ff8a65", label: "Max local (q:NN)", shape: "line" },
  { swatch: "#81c784", label: "Active tasks", shape: "line" },
];
