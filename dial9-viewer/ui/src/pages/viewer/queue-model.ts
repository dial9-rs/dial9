// src/pages/viewer/queue-model.ts - the Queue depth track's pure model
// (T29; docs/ui-inventory/features/02-viewer-html.md section M; legacy
// `renderQueueChart` / `showSpawnedTasks` in viewer.html). NODE-TESTABLE: no
// store, no DOM, no canvas - everything the queue-track controller
// (queue-track.ts) needs to derive its series, scale them, and answer the M7
// drag-select lives here so the numbers (J7 behavioral-diff) and the S6 scale
// fix are unit-tested directly.
//
// S6 (issue #282, the core correctness ask): the legacy global-queue area was
// a filled step whose top edge sat at the very chart bottom when the value was
// 0 - a 1px stroke fused to the axis, indistinguishable from "no data"
// ("invisible at zero", which confused a live Tokioconf audience). `queueScaleY`
// reserves ZERO_BASELINE_PX below the lowest data so a 0-valued series maps to
// a VISIBLE flat line a fixed distance above the axis. The underlying numbers
// (bucketed max global/local, active-task counts, maxQ) are IDENTICAL to the
// legacy chart - only the y-mapping changes (the ledgered presentation delta).
//
// The three series (global injection queue, max per-worker local queue,
// active-task count) share ONE explicit zero baseline (the S6 "one labeled,
// always-visible representation") even though the active-task line keeps its
// own right-axis magnitude scale, so all three read against the same floor.

import type { ParsedTrace, TimeRange } from "../../types/trace.js";
import type { StoreState } from "../../types/state.js";
import { buildActiveTaskTimeline, buildWorkerSpans } from "../../lib/trace/index.js";
import { deriveWorkerIds } from "../../components/canvas/lanes/data.js";

// ── Windowing descriptor (T17 carried obligation) ────────────────────────

/**
 * The resident-window state the queue renderer must surface so a truncated /
 * partial window is never painted as complete (T17-audit notes 6+7). Both
 * fields resolve to the "complete" value for a whole-trace load. Mirrors the
 * CPU track's `CpuWindow` (cpu.ts) and the task-detail track's window.
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

/** One entry of the merged, sorted local-queue timeline (legacy mergedLocalSamples). */
export interface MergedLocalSample {
  t: number;
  /** Worker the sample belongs to. */
  w: number;
  local: number;
}

/**
 * Everything the queue track needs for one render pass + the M7 drag-select,
 * lifted out of the store so the render logic stays Node-testable. Built once
 * per loaded trace by `computeQueueData` (store.derived over the `trace`
 * slice), never per frame.
 */
export interface QueueData {
  /** Worker ids in render order (same set the lanes/overlay use). */
  workerIds: readonly number[];
  /** Global injection-queue series, sorted by t (M1 / I6 Global Q). */
  queueSamples: readonly { t: number; global: number }[];
  /**
   * All per-worker local-queue samples merged into one t-sorted timeline
   * (legacy mergedLocalSamples), so the render steps it once instead of
   * re-merging per frame (M2 max-local line).
   */
  mergedLocalSamples: readonly MergedLocalSample[];
  /** Active-task-count timeline, sorted by t (M3 / I6 Active Tasks). */
  activeTaskSamples: readonly { t: number; count: number }[];
  /** task id -> first-poll time (legacy taskFirstPoll), the M7 spawn proxy. */
  taskFirstPoll: ReadonlyMap<number, number>;
  /** task id -> spawn-location id (null when unknown). */
  taskSpawnLocs: ReadonlyMap<number, string | null>;
  /** spawn-location id -> human-readable location. */
  spawnLocations: ReadonlyMap<string, string>;
  /** True when the trace carries the active-task timeline (M3/M7 gate). */
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
 * Derive the queue track's frame-invariant data for one parsed trace. Runs the
 * frozen core's buildWorkerSpans (global + per-worker queue series) and
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
  // ONCE here so the per-frame render never re-merges (legacy built this at
  // load time as `mergedLocalSamples`).
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
 * Derive the T17 window descriptor from the store (carried obligation): any
 * segment stuck "oversized" makes the queue view unavoidably partial. The
 * whole-trace shell has an empty segments slice, so this resolves to complete;
 * the `truncatedAt` edge derivation from a live resident window is the
 * downstream wiring seam (T34/T35), and the renderer consumes it regardless.
 * Mirrors cpu.ts `deriveCpuWindow`.
 */
export function deriveQueueWindow(state: StoreState): QueueWindow {
  for (const entry of state.segments.segments.values()) {
    if (entry.state === "oversized") {
      return { truncatedAt: null, oversized: true };
    }
  }
  return COMPLETE_QUEUE_WINDOW;
}

// ── The S6 scale function (the Vitest-tested crux) ───────────────────────

/**
 * Pixels reserved between the chart's true bottom (the axis) and the y a
 * value of 0 maps to. This is the S6 / #282 fix: a 0-valued series (a global
 * queue that sits at 0 for the whole window) renders as a VISIBLE flat line
 * this far above the axis instead of a stroke fused to it. Small enough to
 * cost almost no vertical range, large enough to read as a distinct line.
 */
export const ZERO_BASELINE_PX = 3;

/**
 * Map a queue-series value in [0, max] to a y coordinate inside the chart band
 * [chartTop, chartTop + chartH]. The bottom ZERO_BASELINE_PX is reserved so
 * value 0 lands a visible distance ABOVE the axis (the explicit zero baseline,
 * S6): `queueScaleY(0, ...)` is strictly less than `chartTop + chartH`. `max`
 * <= 0 pins everything to that baseline. The result is clamped into the band.
 *
 * This is the single scale every series (global, max-local, active-task) runs
 * through, so a 0-valued global renders the same visible flat line the legacy
 * chart hid at the axis.
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

/** The y a value of 0 maps to (the visible zero baseline; S6). */
export function queueBaselineY(
  chartTop: number,
  chartH: number,
  baselinePx: number = ZERO_BASELINE_PX,
): number {
  return chartTop + chartH - Math.min(Math.max(0, baselinePx), chartH);
}

// ── Per-frame render model (bucketing; the J7 parity target) ─────────────

/** The active-task overlay (M3): a step line on its own right-axis magnitude. */
export interface QueueActiveTaskModel {
  /** Step-line vertices in draw-area x (px) + raw count (y via queueScaleY). */
  points: readonly { x: number; count: number }[];
  /** Count at viewStart (the step line's left seed), legacy countAtViewStart. */
  startCount: number;
  /** Right-axis magnitude (legacy maxTasks), >= 1. */
  maxTasks: number;
}

/**
 * The bucketed queue render model for one frame. `global[i]` / `local[i]` are
 * the plotted (carry-forward-applied) step values at pixel column i, exactly
 * the legacy per-bucket values - the y-mapping (queueScaleY) is the only thing
 * that changed for S6, so these numbers behavioral-diff against the legacy
 * chart. `maxQ` is the shared global+local magnitude (legacy: max(1, ...)).
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
 * one view window. Ports `renderQueueChart`'s bucketing verbatim (viewer.html:
 * 4748-4891) so the plotted numbers are identical to legacy (J7): global takes
 * the max sample per pixel bucket; local steps the merged timeline tracking
 * each worker's current depth and takes the max across workers per bucket;
 * both carry the last value forward across empty buckets. `maxQ` is
 * max(1, peak global, peak local). The active-task overlay reproduces
 * legacy maxTasks + countAtViewStart on its own scale.
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

  // ── Global queue: max sample per pixel bucket (legacy 4755-4769) ────────
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

  // ── Local queue: single pass over the merged timeline (legacy 4771-4800) ─
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

  // maxQ across visible buckets (legacy 4802-4807).
  let maxQ = 1;
  for (let i = 0; i < numBuckets; i++) {
    if (bucketGlobal[i]! > maxQ) maxQ = bucketGlobal[i]!;
    if (bucketLocal[i]! > maxQ) maxQ = bucketLocal[i]!;
  }

  // Carry-forward so global[i]/local[i] are the plotted step value at pixel i
  // (legacy tracked lastY, updating only on hasData buckets; the value carries
  // otherwise). Start at 0 (the baseline) until the first data bucket.
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
 * The active-task step-line model (M3), ported from legacy 4858-4891:
 * maxTasks = max(1, peak count in view, count at viewStart); the step line is
 * seeded with countAtViewStart and one vertex per in-view sample, at draw-area
 * x. Null when the trace has no active-task timeline (M3 CONDITIONAL).
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

// ── M7 drag-select: tasks spawned in a time range (the T31 render source) ─

/** One spawn-location group of tasks first polled inside the M7 range. */
export interface SpawnedTaskGroup {
  /** Spawn location (or "(unknown)"). */
  loc: string;
  /** Tasks in this group, in taskFirstPoll iteration order (legacy order). */
  tasks: readonly { taskId: number; firstPoll: number }[];
}

/**
 * The M7 drag-select result: tasks whose FIRST POLL falls in `range`, grouped
 * by spawn location and sorted by count desc. This is the DERIVATION half of
 * M7 - T29 dispatches the range to `selection.spawnedTasksRange`; T31's
 * inspector RENDERS this (the clickable hex task-id links, 5 per group + the
 * "N more" tail, and the range duration are T31's presentation). Kept here (a
 * pure port of legacy `showSpawnedTasks`, viewer.html:7147-7188, minus the DOM)
 * so the numbers behavioral-diff against legacy (J7) and T31 reuses the same
 * logic instead of reimplementing it.
 */
export interface SpawnedTasksResult {
  range: TimeRange;
  /** Total tasks found in range (legacy `tasks.length`). */
  total: number;
  /** Groups sorted by task count desc (legacy sort). */
  groups: readonly SpawnedTaskGroup[];
}

/**
 * Compute the M7 spawned-task groups for a time range, or null when no task
 * was first polled in range (legacy `if (!tasks.length) return;`). Uses
 * `taskFirstPoll` as the spawn proxy exactly as legacy did (NOT taskSpawnTimes)
 * so the counts match. Bounds are inclusive on both ends (legacy `>=`/`<=`).
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
    const loc =
      locId != null ? data.spawnLocations.get(locId) ?? "(unknown)" : "(unknown)";
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

// ── Legend (F3: the queue legend covers every in-track encoding) ─────────

/** One queue-legend entry: a swatch encoding + its meaning (matches the draw). */
export interface QueueLegendEntry {
  /** Swatch fill (CSS color). */
  swatch: string;
  label: string;
  /** Shape hint so the swatch reads as an area/line, matching the render. */
  shape: "area" | "line";
}

/**
 * The queue track legend (F3 amendment): every series the track draws is
 * explained, with swatch encodings that MATCH the in-track rendering (the
 * legacy header legend omitted the encodings and died with the collapsed fold;
 * S1). Ordered global -> local -> active-task, the draw order.
 */
export const QUEUE_LEGEND: readonly QueueLegendEntry[] = [
  { swatch: "#4fc3f7", label: "Global queue", shape: "area" },
  { swatch: "#ff8a65", label: "Max local (q:NN)", shape: "line" },
  { swatch: "#81c784", label: "Active tasks", shape: "line" },
];
