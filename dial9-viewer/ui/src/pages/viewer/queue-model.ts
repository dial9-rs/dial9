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
import { buildActiveTaskTimeline } from "../../lib/trace/index.js";
import {
  deriveRuntimeMetrics,
  deriveWorkerIds,
  sharedWorkerSpans,
} from "../../lib/trace/derived.js";

// ── Windowing descriptor ─────────────────────────────────────────────────

/**
 * The resident-window state the queue renderer must surface so a truncated /
 * partial window is never painted as complete. Both fields resolve to the
 * "complete" value for a whole-trace load. Mirrors the CPU track's `CpuWindow`.
 */


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
  const spanResult = sharedWorkerSpans(trace);
  const runtimeMetrics = deriveRuntimeMetrics(trace);
  const timeline = buildActiveTaskTimeline(
    trace.taskSpawnTimes,
    trace.taskTerminateTimes,
  );
  // The global-queue series comes from per-runtime RuntimeMetrics (summed per
  // cycle) when the trace has them; otherwise fall back to the legacy
  // pre-summed QueueSample series that buildWorkerSpans extracts.
  const queueSamples = runtimeMetrics.present
    ? runtimeMetrics.summedGlobalQueue
    : spanResult.queueSamples;

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
    queueSamples,
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

/**
 * Running max over the workers' current local depths.
 *
 * The queue track's cost used to be O(pixels x workers): every pixel column
 * re-scanned every worker's depth to find the max, ~48k map reads per frame at
 * 32 workers, and it did that whether or not anything was in view. That is why
 * this track janked where spans/events (which scale with visible data) did not.
 *
 * Depths are small non-negative integers, so a histogram over depth gives O(1)
 * updates: bump the counts, raise the max on the way up, and walk it down only
 * when the current max empties. The walk-down is amortized against the raises,
 * so a full sweep is O(pixels + transitions) instead of O(pixels x workers).
 */
class RunningMax {
  /** countAtDepth[d] = how many workers currently sit at depth d. */
  private counts = new Int32Array(64);
  private depthOf = new Map<number, number>();
  private max = 0;

  reset(workerIds: readonly number[]): void {
    this.counts.fill(0);
    this.depthOf.clear();
    this.max = 0;
    // Every worker starts at depth 0, matching the previous seed loop.
    if (workerIds.length > 0) this.counts[0] = workerIds.length;
    for (const w of workerIds) this.depthOf.set(w, 0);
  }

  /**
   * Record a worker's new depth. Workers not seeded by `reset` are IGNORED: the
   * merged timeline can carry samples for workers outside `data.workerIds`, and
   * the previous implementation took its max by iterating that id list, so such
   * samples were applied to its state map but never counted. Dropping them here
   * preserves that, or the plotted max would silently grow.
   */

  private grow(depth: number): void {
    let n = this.counts.length;
    while (n <= depth) n *= 2;
    const next = new Int32Array(n);
    next.set(this.counts);
    this.counts = next;
  }

  set(worker: number, depth: number): void {
    const prev = this.depthOf.get(worker);
    if (prev === undefined) return; // untracked worker - see tracks()
    const d = depth < 0 ? 0 : depth;
    if (d >= this.counts.length) this.grow(d);
    if (prev === d) return;
    this.counts[prev]! -= 1;
    this.counts[d]! += 1;
    this.depthOf.set(worker, d);
    if (d > this.max) {
      this.max = d;
      return;
    }
    // Only a drop from the current peak can lower the max, and only once that
    // depth is vacated.
    if (prev === this.max && this.counts[prev]! === 0) {
      let m = this.max;
      while (m > 0 && this.counts[m]! === 0) m--;
      this.max = m;
    }
  }

  value(): number {
    return this.max;
  }
}

// Frame-scratch, reused across paints: at ~1500 buckets these were five fresh
// arrays per frame, on a path that runs on every pan/zoom/minimap-scrub frame.
// Grown on demand, never shrunk; only ever read back within one synchronous
// buildQueueRenderModel call, so reuse is not observable to callers.
let scratchGlobal = new Float64Array(0);
let scratchLocal = new Float64Array(0);
let scratchHasData = new Uint8Array(0);
const runningMax = new RunningMax();

function ensureScratch(n: number): void {
  if (scratchGlobal.length >= n) {
    scratchGlobal.fill(0, 0, n);
    scratchLocal.fill(0, 0, n);
    scratchHasData.fill(0, 0, n);
    return;
  }
  scratchGlobal = new Float64Array(n);
  scratchLocal = new Float64Array(n);
  scratchHasData = new Uint8Array(n);
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

  ensureScratch(numBuckets);
  const bucketGlobal = scratchGlobal;
  const bucketLocal = scratchLocal;
  const bucketHasData = scratchHasData;

  // ── Global queue: max sample per pixel bucket ───────────────────────────
  {
    const startIdx = Math.max(0, lowerBoundT(data.queueSamples, viewStart) - 1);
    for (let i = startIdx; i < data.queueSamples.length; i++) {
      const s = data.queueSamples[i]!;
      if (s.t > viewEnd) break;
      if (s.t < viewStart) continue;
      const bi = Math.floor(((s.t - viewStart) / viewDur) * (numBuckets - 1));
      if (bi < 0 || bi >= numBuckets) continue;
      bucketHasData[bi] = 1;
      if (s.global > bucketGlobal[bi]!) bucketGlobal[bi] = s.global;
      hasData = true;
    }
  }

  // ── Local queue: single pass over the merged timeline ───────────────────
  const merged = data.mergedLocalSamples;
  runningMax.reset(data.workerIds);
  // Seed each worker from the sample just before viewStart.
  const mergeStart = Math.max(0, lowerBoundT(merged, viewStart) - 1);
  for (let i = mergeStart; i < merged.length; i++) {
    const s = merged[i]!;
    if (s.t >= viewStart) break;
    runningMax.set(s.w, s.local);
  }
  let sampleIdx = Math.max(mergeStart, lowerBoundT(merged, viewStart));
  for (let bi = 0; bi < numBuckets; bi++) {
    const bucketEnd = viewStart + ((bi + 1) / numBuckets) * viewDur;
    while (sampleIdx < merged.length && merged[sampleIdx]!.t < bucketEnd) {
      const s = merged[sampleIdx++]!;
      runningMax.set(s.w, s.local);
      bucketHasData[bi] = 1;
      hasData = true;
    }
    bucketLocal[bi] = runningMax.value();
  }

  // maxQ across visible buckets, and the carry-forward that makes
  // global[i]/local[i] the plotted step value at pixel i: update only on
  // hasData buckets, else carry the last value forward. Starts at 0 (the
  // baseline) until the first data bucket.
  let maxQ = 1;
  let lastG = 0;
  let lastL = 0;
  for (let i = 0; i < numBuckets; i++) {
    if (bucketGlobal[i]! > maxQ) maxQ = bucketGlobal[i]!;
    if (bucketLocal[i]! > maxQ) maxQ = bucketLocal[i]!;
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
  const n = samples.length;
  if (n === 0) return null;

  // Bounded window: samples are t-sorted, so seek the edges by binary search
  // instead of scanning all ~O(tasks) samples every frame. `startCount` is the
  // level entering the view = the last sample with t <= viewStart (upperBound-1);
  // a sample exactly at viewStart seeds AND becomes the first point, matching the
  // old scan.
  let a = 0, b = n;
  while (a < b) { const m = (a + b) >>> 1; if (samples[m]!.t <= viewStart) a = m + 1; else b = m; }
  const startCount = a > 0 ? samples[a - 1]!.count : 0;

  const from = lowerBoundT(samples, viewStart);
  let maxTasks = Math.max(1, startCount);
  const points: { x: number; count: number }[] = [];
  for (let i = from; i < n; i++) {
    const s = samples[i]!;
    if (s.t > viewEnd) break;
    if (s.count > maxTasks) maxTasks = s.count;
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

export {
  COMPLETE_WINDOW as COMPLETE_QUEUE_WINDOW,
  deriveResidentWindow as deriveQueueWindow,
  type ResidentWindow as QueueWindow,
} from "./resident-window.js";
