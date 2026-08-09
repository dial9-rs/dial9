// The task-index model behind the rail's Tasks tab: one row per task in the
// trace, aggregated once per load and sorted client-side. Mirrors poi.ts - a
// WeakMap-memoized source built off the columnar store, plus a pure display
// sort - so the rail treats Issues and Tasks the same way.
//
// The aggregation reads the columnar store's taskAggregates() (poll count /
// total / longest / first-poll, straight off the columns, no PollView), then
// joins the trace's per-task maps. Every map field is read defensively: old or
// partial traces legitimately miss keys, so a missing value becomes null / the
// domain default, never NaN (AGENTS.md trace-format rules).

import { sharedWorkerSpans, columnarWorkerStoreFor } from "../../lib/trace/derived.js";
import {
  laneSource,
  type TaskPollAggregate,
} from "../../lib/trace/columnar-worker-spans.js";
import { formatHumanDuration } from "../../lib/trace/index.js";
import { railWindow } from "./poi.js";
import type { ParsedTrace, PollSpan, WorkerLane } from "../../types/trace.js";
import type { PoiSlice } from "../../types/state.js";

/** One task as the Tasks tab lists it. */
export interface TaskIndexRow {
  taskId: number;
  /** Resolved spawn location, or null when unknown / not recorded. */
  spawnLoc: string | null;
  spawnTs: number | null;
  terminateTs: number | null;
  /** terminate - spawn, only when both are known. */
  lifetimeNs: number | null;
  /** Defaults to true (matches computeTaskDetailData) on old traces without the
   *  instrumentation map. */
  instrumented: boolean;
  pollCount: number;
  totalPollNs: number;
  longestPollNs: number;
  /** Start of the task's earliest poll; -1 for a spawned-but-never-polled task.
   *  The chronological default sort key. */
  firstPollStart: number;
  /** Worker the earliest poll ran on; -1 when never polled. The reveal target. */
  firstPollWorker: number;
  workerCount: number;
}

export interface TaskIndex {
  rows: TaskIndexRow[];
}

export type TaskSortKey =
  | "id"
  | "loc"
  | "polls"
  | "total"
  | "longest"
  | "lifetime";

const indexCache = new WeakMap<ParsedTrace, TaskIndex>();

/**
 * The memoized task index for a trace (built once per load). Sources the
 * per-task poll aggregates from the columnar store when present (raw-column
 * reduction, no PollView), else a fat-lane fallback for small traces / mocks.
 * Tasks that never polled are unioned in from the spawn-time map with a zero
 * poll count, so "all tasks" genuinely means all.
 */
export function taskIndexFor(trace: ParsedTrace): TaskIndex {
  const cached = indexCache.get(trace);
  if (cached !== undefined) return cached;

  const spanResult = sharedWorkerSpans(trace);
  const lanes = laneSource(columnarWorkerStoreFor(trace), spanResult.workerSpans);
  const aggregates = lanes.columnar
    ? lanes.store.taskAggregates()
    : fatTaskAggregates(lanes.workerSpans);

  const byTask = new Map<number, TaskPollAggregate>();
  for (const a of aggregates) byTask.set(a.taskId, a);

  // Union: every task that polled, plus every task that spawned (even if it
  // never ran a poll). A spawned-but-never-polled task lists with pollCount 0.
  const taskIds = new Set<number>(byTask.keys());
  for (const id of trace.taskSpawnTimes.keys()) taskIds.add(id);

  const rows: TaskIndexRow[] = [];
  for (const taskId of taskIds) {
    const agg = byTask.get(taskId);
    const spawnLocId = trace.taskSpawnLocs.get(taskId);
    const spawnLoc =
      spawnLocId != null ? trace.spawnLocations.get(spawnLocId) ?? null : null;
    const spawnTs = trace.taskSpawnTimes.get(taskId) ?? null;
    const terminateTs = trace.taskTerminateTimes.get(taskId) ?? null;
    const lifetimeNs =
      terminateTs != null && spawnTs != null ? terminateTs - spawnTs : null;
    rows.push({
      taskId,
      spawnLoc,
      spawnTs,
      terminateTs,
      lifetimeNs,
      instrumented: trace.taskInstrumented.get(taskId) ?? true,
      pollCount: agg?.pollCount ?? 0,
      totalPollNs: agg?.totalPollNs ?? 0,
      longestPollNs: agg?.longestPollNs ?? 0,
      firstPollStart: agg?.firstPollStart ?? -1,
      firstPollWorker: agg?.firstPollWorker ?? -1,
      workerCount: agg?.workerCount ?? 0,
    });
  }

  const index: TaskIndex = { rows };
  indexCache.set(trace, index);
  return index;
}

/** Fat-lane fallback: the same aggregation over PollSpan[] for small traces. */
function fatTaskAggregates(
  workerSpans: Record<number, WorkerLane>,
): TaskPollAggregate[] {
  interface Acc {
    pollCount: number;
    totalPollNs: number;
    longestPollNs: number;
    firstPollStart: number;
    firstPollWorker: number;
    workers: Set<number>;
  }
  const acc = new Map<number, Acc>();
  for (const key of Object.keys(workerSpans)) {
    const w = Number(key);
    const lane = workerSpans[w];
    if (!lane) continue;
    for (const p of lane.polls as PollSpan[]) {
      const dur = p.end - p.start;
      let a = acc.get(p.taskId);
      if (a === undefined) {
        a = {
          pollCount: 0,
          totalPollNs: 0,
          longestPollNs: 0,
          firstPollStart: p.start,
          firstPollWorker: w,
          workers: new Set<number>(),
        };
        acc.set(p.taskId, a);
      }
      a.pollCount++;
      a.totalPollNs += dur;
      if (dur > a.longestPollNs) a.longestPollNs = dur;
      if (p.start < a.firstPollStart) {
        a.firstPollStart = p.start;
        a.firstPollWorker = w;
      }
      a.workers.add(w);
    }
  }
  const out: TaskPollAggregate[] = [];
  for (const [taskId, a] of acc) {
    out.push({
      taskId,
      pollCount: a.pollCount,
      totalPollNs: a.totalPollNs,
      longestPollNs: a.longestPollNs,
      firstPollStart: a.firstPollStart,
      firstPollWorker: a.firstPollWorker,
      workerCount: a.workers.size,
    });
  }
  return out;
}

// ── Display sort ─────────────────────────────────────────────────────────

/**
 * A stable client-side sort of the task rows by the chosen column. Returns a
 * NEW array (never mutates the memoized index). Ties fall back to task id
 * ascending for determinism.
 */
export function sortTasks(
  rows: readonly TaskIndexRow[],
  sortKey: TaskSortKey,
  sortDir: "asc" | "desc",
): TaskIndexRow[] {
  const dir = sortDir === "asc" ? 1 : -1;
  const keyed = rows.map((row, i) => ({ row, i }));
  keyed.sort((a, b) => {
    const cmp = compareTasks(a.row, b.row, sortKey);
    if (cmp !== 0) return cmp * dir;
    // Stable tiebreak on task id, so equal rows never jitter.
    return a.row.taskId - b.row.taskId;
  });
  return keyed.map((k) => k.row);
}

function compareTasks(a: TaskIndexRow, b: TaskIndexRow, key: TaskSortKey): number {
  switch (key) {
    case "id":
      return a.taskId - b.taskId;
    case "loc":
      return (a.spawnLoc ?? "").localeCompare(b.spawnLoc ?? "");
    case "polls":
      return a.pollCount - b.pollCount;
    case "total":
      return a.totalPollNs - b.totalPollNs;
    case "longest":
      return a.longestPollNs - b.longestPollNs;
    case "lifetime":
      // Unknown lifetimes sort as -1 so they cluster at the low end.
      return (a.lifetimeNs ?? -1) - (b.lifetimeNs ?? -1);
  }
}

// ── Row formatting ─────────────────────────────────────────────────────────

/** `0x{hex}` task id label. */
export function taskIdLabel(taskId: number): string {
  return `0x${taskId.toString(16)}`;
}

/** A poll-time / lifetime column, or an em-dash-free placeholder when absent. */
export function durationCell(ns: number | null): string {
  if (ns == null || ns < 0) return "-";
  return formatHumanDuration(ns);
}

// ── View model (the rail's Tasks-tab render input) ────────────────────────

/** One formatted task row. `task` is the underlying data (for the jump). */
export interface TaskRow {
  task: TaskIndexRow;
  id: string;
  loc: string;
  polls: string;
  total: string;
  longest: string;
  lifetime: string;
}

export interface TaskViewModel {
  sortKey: TaskSortKey;
  sortDir: "asc" | "desc";
  /** Absolute index of the current task in `sorted`, or -1. */
  index: number;
  /** The formatted window (at most RAIL_WINDOW rows). Row i is `windowStart+i`. */
  rows: TaskRow[];
  windowStart: number;
  /** The full sorted list; `n`/`p` step across all of it. */
  sorted: readonly TaskIndexRow[];
  /** Total task count (the "N/total" position). */
  total: number;
  /**
   * False when this list covers only dial9-spawned tasks. Drives the empty
   * state, which then explains why the list may be short.
   */
  hasFullTaskCoverage: boolean;
  /**
   * "none": no spawn events, "-" `life` cells mean "not captured", not
   * "instant". "partial": some rows captured, some not.
   */
  taskLifetimeCoverage: "all" | "partial" | "none";
}

/**
 * Build the Tasks-tab view model: the whole task index sorted by the chosen
 * column, with only the window around the cursor formatted (the list is
 * unbounded, so formatting every row would be the same tab-killer the POI
 * window guards against).
 */
export function deriveTaskViewModel(
  trace: ParsedTrace | null,
  poi: PoiSlice,
): TaskViewModel {
  if (trace === null) {
    return {
      sortKey: poi.taskSort,
      sortDir: poi.taskSortDir,
      index: -1,
      rows: [],
      windowStart: 0,
      sorted: [],
      total: 0,
      hasFullTaskCoverage: true,
      taskLifetimeCoverage: "all",
    };
  }
  const sorted = sortTasks(taskIndexFor(trace).rows, poi.taskSort, poi.taskSortDir);
  const index = poi.taskIndex < sorted.length ? poi.taskIndex : -1;
  const { start, end } = railWindow(sorted.length, index);
  const rows: TaskRow[] = [];
  for (let i = start; i < end; i++) {
    const task = sorted[i]!;
    rows.push({
      task,
      id: taskIdLabel(task.taskId),
      loc: task.spawnLoc ?? "-",
      polls: task.pollCount.toLocaleString(),
      total: durationCell(task.totalPollNs),
      longest: durationCell(task.longestPollNs),
      lifetime: durationCell(task.lifetimeNs),
    });
  }
  return {
    sortKey: poi.taskSort,
    sortDir: poi.taskSortDir,
    index,
    rows,
    windowStart: start,
    hasFullTaskCoverage: trace.hasFullTaskCoverage,
    taskLifetimeCoverage: !trace.hasTaskLifetimes
      ? "none"
      : sorted.some((t) => t.lifetimeNs === null)
        ? "partial"
        : "all",
    sorted,
    total: sorted.length,
  };
}
