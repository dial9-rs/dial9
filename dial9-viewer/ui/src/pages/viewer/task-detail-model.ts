// The task-detail track's pure derivation + per-frame render model. No store, no
// DOM, no canvas - every function is referentially transparent.
//
// Two tiers:
//   1. computeTaskDetailData(source, trace, taskId) - selection-invariant given
//      the selected task: collect the task's polls across all workers (sorted by
//      start), its wakes, the per-poll wake match (computePollWakes), the
//      lifetime/spawn/terminate numbers, the waker labels, and the task-dump
//      references. This is both the timeline track's input and the data the
//      inspector Task tab renders. Cached in a store
//      `derived(["trace","selection"], ...)` - a selection change invalidates, a
//      pan does not.
//   2. buildTaskDetailRenderModel(...) - viewport-dependent: the visible poll
//      window, scheduling-delay bands, poll bars / coverage histogram, idle
//      gaps, the lifespan bar, and the hit/wake regions the interaction layer
//      reads. Memoized on its primitive inputs so a hover-only redraw never
//      re-walks the collection.
//
// A segment-windowed collection can be missing the task's polls beyond the
// resident window. The render carries a TaskDetailWindow descriptor and surfaces
// a truncated/oversized window rather than presenting a clipped poll list as the
// task's whole history.

import {
  EVENT_TYPES,
  computePollWakes,
  computeRuntimeGroups,
  formatHumanDuration,
} from "../../lib/trace/index.js";
import type {
  ParsedTrace,
  PollSpan,
  SpanList,
  TaskDump,
  TaskWake,
  LaneSpans,
} from "../../lib/trace/index.js";
import { nsToDrawX, pixelCoverage } from "../../lib/canvas/index.js";
import { lifecycleWorkerIds, sharedWorkerSpans } from "../../lib/trace/derived.js";
import {
  isColumnarLane,
  type ColumnarWorkerSpans,
} from "../../lib/trace/columnar-worker-spans.js";

// ── Trace-invariant poll source (buildWorkerSpans, keyed by trace) ────────

/**
 * The per-worker poll lanes + wake index the task-detail collection reads,
 * derived once per trace (buildWorkerSpans scans every event). The component
 * wraps this in a store `derived(["trace"], ...)` so it is NOT rebuilt when the
 * selected task or the viewport changes.
 */
export interface TaskPollSource {
  /** Distinct runtime worker ids (render order irrelevant here). */
  workerIds: readonly number[];
  /** Reconstructed poll/park/active spans per worker. */
  workerSpans: Record<number, LaneSpans>;
  /** Wake events indexed by woken task. */
  wakesByTask: Record<number, TaskWake[]>;
}

/** Empty source (no trace) so callers never special-case null. */
export const EMPTY_TASK_POLL_SOURCE: TaskPollSource = {
  workerIds: [],
  workerSpans: {},
  wakesByTask: {},
};

/**
 * The distinct worker ids in a trace, in runtime-group order (mirrors the lanes'
 * deriveWorkerIds without coupling to that module): scan non-queue, non-wake
 * events for the worker set, then reorder by runtime groups. Only the SET and
 * its COUNT matter for task-detail (poll collection + the "io" waker
 * discriminant), but the grouped order is kept.
 */
export function collectWorkerIds(trace: ParsedTrace): number[] {
  return computeRuntimeGroups(
    lifecycleWorkerIds(trace),
    trace.runtimeWorkers,
  ).flatMap((g) => g.workerIds);
}

/**
 * Build the trace-invariant poll source (buildWorkerSpans + wake index).
 * Returns EMPTY_TASK_POLL_SOURCE for a null/eventless trace.
 */
export function buildTaskPollSource(trace: ParsedTrace | null): TaskPollSource {
  if (trace === null || trace.maxTs === null) return EMPTY_TASK_POLL_SOURCE;
  const workerIds = collectWorkerIds(trace);
  const result = sharedWorkerSpans(trace);
  return {
    workerIds,
    workerSpans: result.workerSpans,
    wakesByTask: result.wakesByTask,
  };
}

// ── Per-task detail data (selection-keyed) ────────────────────────────────

/**
 * One poll's matched wake (computePollWakes entry) plus the resolved waker
 * label, precomputed here because it depends only on the trace + wake
 * (viewport-invariant) - so the per-frame render model never re-resolves spawn
 * locations while panning.
 */
export interface PollWakeInfo {
  wake: TaskWake;
  /** The wake time bumped past an earlier poll that contained it. */
  effectiveWake: number;
  /** Label: "io" for runtime/worker wakes, else spawn filename / hex id. */
  wakerLabel: string;
}

/**
 * Everything about ONE selected task that does NOT depend on the viewport - the
 * timeline track's input AND the data the inspector Task tab renders. Cached in
 * a store `derived(["trace","selection"], ...)`.
 */
export interface TaskDetailData {
  /** The selected task, or null when nothing is selected. */
  taskId: number | null;
  /** All polls for the task across every worker, sorted by start. */
  polls: readonly PollSpan[];
  /** The task's wake events (wake count source). */
  wakes: readonly TaskWake[];
  /** Per-poll wake match + resolved label, parallel to `polls`. */
  pollWakes: readonly (PollWakeInfo | null)[];
  /** Poll count. */
  pollCount: number;
  /** Wake count (shown only when instrumented). */
  wakeCount: number;
  /** Resolved spawn location string for the task, or null. */
  spawnLocation: string | null;
  /** False for tasks spawned via raw tokio::spawn. */
  isInstrumented: boolean;
  /** Spawn timestamp (ns), or null when untracked. */
  spawnTs: number | null;
  /** Terminate timestamp (ns), or null when the task did not finish. */
  terminateTs: number | null;
  /** True when a terminate time exists (the completion mark). */
  hasTerminate: boolean;
  /** terminate - spawn (ns), or null when either is missing. */
  lifetimeNs: number | null;
  /** Async-stack captures for the task, sorted by ts. */
  taskDumps: readonly TaskDump[];
  /** Distinct worker count (the "io" waker discriminant). */
  workerIdCount: number;
  /** True once the task has >= 1 poll - the track-visibility predicate. */
  hasPolls: boolean;
}

/** Empty resting data (no task selected). */
export const EMPTY_TASK_DETAIL_DATA: TaskDetailData = {
  taskId: null,
  polls: [],
  wakes: [],
  pollWakes: [],
  pollCount: 0,
  wakeCount: 0,
  spawnLocation: null,
  isInstrumented: true,
  spawnTs: null,
  terminateTs: null,
  hasTerminate: false,
  lifetimeNs: null,
  taskDumps: [],
  workerIdCount: 0,
  hasPolls: false,
};

/**
 * Resolve the waker label for a wake's `wakerTaskId`: runtime/worker wakes
 * (falsy, 0, or in the worker-id range) show "io"; otherwise the waker task's
 * spawn filename, falling back to its hex id.
 */
export function wakerLabelFor(
  wakerTaskId: number,
  trace: Pick<ParsedTrace, "taskSpawnLocs" | "spawnLocations">,
  workerIdCount: number,
): string {
  if (
    !wakerTaskId ||
    wakerTaskId === 0 ||
    (wakerTaskId >= 1 && wakerTaskId <= workerIdCount)
  ) {
    return "io";
  }
  const wakerLoc = trace.taskSpawnLocs.get(wakerTaskId);
  const wakerLocStr =
    wakerLoc != null ? trace.spawnLocations.get(wakerLoc) : null;
  return wakerLocStr != null
    ? wakerLocStr.replace(/.*\//, "")
    : `task 0x${wakerTaskId.toString(16)}`;
}

/**
 * Collect the selection-keyed detail for `taskId`: gather the task's polls
 * across every worker and sort by start; match wakes with computePollWakes; read
 * the numbers from the trace's task maps. Returns EMPTY_TASK_DETAIL_DATA when
 * nothing is selected or the task has no polls (the "hide on deselect / no
 * polls" contract).
 */
export function computeTaskDetailData(
  source: TaskPollSource,
  trace: ParsedTrace | null,
  taskId: number | null,
): TaskDetailData {
  if (taskId === null || trace === null) return EMPTY_TASK_DETAIL_DATA;

  // Collect all polls for this task across all workers, sorted by start. On the
  // columnar path scan the raw taskId columns (materializing only this task's
  // polls) instead of iterating every flyweight lane view. The store rides on
  // the lane views themselves, so we read it from `source` (no trace re-derive).
  let store: ColumnarWorkerSpans | undefined;
  for (const w of source.workerIds) {
    const lane = source.workerSpans[w];
    if (lane !== undefined && isColumnarLane(lane)) { store = lane.store; break; }
  }
  let polls: PollSpan[];
  if (store) {
    polls = store.pollsForTask(taskId);
  } else {
    polls = [];
    for (const w of source.workerIds) {
      const lane = source.workerSpans[w];
      if (lane === undefined) continue;
      for (const s of lane.polls) {
        if (s.taskId === taskId) polls.push(s);
      }
    }
    polls.sort((a, b) => a.start - b.start);
  }
  if (polls.length < 1) return EMPTY_TASK_DETAIL_DATA;

  const wakes = source.wakesByTask[taskId] ?? [];
  const rawPollWakes = computePollWakes(polls, wakes);
  const workerIdCount = source.workerIds.length;
  const pollWakes: (PollWakeInfo | null)[] = rawPollWakes.map((pw) =>
    pw === null
      ? null
      : {
          wake: pw.wake,
          effectiveWake: pw.effectiveWake,
          wakerLabel: wakerLabelFor(pw.wake.wakerTaskId, trace, workerIdCount),
        },
  );

  const firstPoll = polls[0]!;
  // spawnLocations is string-keyed; the frozen builder's numeric 0 fallback
  // means "no recorded location" and has no entry to find.
  const spawnLocId = firstPoll.spawnLocId;
  const spawnLocation =
    typeof spawnLocId === "string" ? trace.spawnLocations.get(spawnLocId) ?? null : null;

  const spawnTs = trace.taskSpawnTimes.get(taskId) ?? null;
  const terminateTs = trace.taskTerminateTimes.get(taskId) ?? null;
  const hasTerminate = terminateTs != null;
  const lifetimeNs =
    hasTerminate && spawnTs != null ? terminateTs - spawnTs : null;
  const isInstrumented = trace.taskInstrumented.get(taskId) ?? true;
  const taskDumps = trace.taskDumps.get(taskId) ?? [];

  return {
    taskId,
    polls,
    wakes,
    pollWakes,
    pollCount: polls.length,
    wakeCount: wakes.length,
    spawnLocation,
    isInstrumented,
    spawnTs,
    terminateTs,
    hasTerminate,
    lifetimeNs,
    taskDumps,
    workerIdCount,
    hasPolls: true,
  };
}

/**
 * The label as PLAIN text (task id, spawn location, poll count, wake count when
 * instrumented, lifetime, completion mark), minus the HTML badges/links (the
 * uninstrumented badge and flamegraph link are the inspector's surface, rendered
 * from the same TaskDetailData).
 */
export function formatTaskDetailSummary(data: TaskDetailData): string {
  if (data.taskId === null) return "";
  let out = `Task 0x${data.taskId.toString(16)}`;
  if (data.spawnLocation != null) out += ` — ${data.spawnLocation}`;
  out += ` · ${data.pollCount} polls`;
  if (data.isInstrumented) out += ` · ${data.wakeCount} wakes`;
  if (data.lifetimeNs != null) {
    out += ` · lifetime ${formatHumanDuration(data.lifetimeNs)}`;
  }
  if (data.hasTerminate) out += " ✓";
  return out;
}

// ── Per-frame render model (viewport-dependent) ───────────────────────────

/** Band vertical placement. */
export const BAND_TOP = 50;
export const BAND_H = 30;

/** Scheduling-delay severity: green / orange / red. */
export type SchedulingSeverity = "low" | "mid" | "high";

interface SchedulingBandBase {
  /** Draw-area-relative x of the delay's starting edge (px). */
  x1: number;
  /** Draw-area-relative x of the poll-start edge (px). */
  x2: number;
  delayNs: number;
  severity: SchedulingSeverity;
  /** Duration label shown only when the band is wider than 25px. */
  showDelayLabel: boolean;
}

/** The delay between task spawn and its first poll. */
export interface SpawnSchedulingBand extends SchedulingBandBase {
  kind: "spawn";
}

/** A wake->poll scheduling delay and its optional visible waker label. */
export interface WakeSchedulingBand extends SchedulingBandBase {
  kind: "wake";
  wakerTaskId: number;
  wakerLabel: string;
  /** Waker label shown (and clickable) only when wider than 40px. */
  showWakerLabel: boolean;
}

export type SchedulingBand = SpawnSchedulingBand | WakeSchedulingBand;

/** A cyan poll bar (per-poll regime). */
export interface PollBar {
  x1: number;
  x2: number;
  durationNs: number;
  /** Per-poll duration label shown only when wider than 35px. */
  showLabel: boolean;
}

/** An idle gap between consecutive polls. */
export interface IdleBand {
  x1: number;
  x2: number;
  durationNs: number;
  /** True when a task dump was captured for this gap (cross-hatch + purple). */
  hasDump: boolean;
  /** The captured dumps (the inspector/flamegraph surface builds from them). */
  dumps: readonly TaskDump[];
  /** Duration label shown only when wider than 35px. */
  showLabel: boolean;
}

/** The observed task lifespan, from spawn through terminate or the visible edge. */
export interface LifespanBar {
  x1: number;
  x2: number;
  /** Draw the spawn edge marker (spawn is inside the view). */
  showSpawn: boolean;
  /** Draw the done edge marker (terminate is inside the view). */
  showDone: boolean;
}

/** A hover status hit region: polling / scheduled / idle. */
export type HitType = "polling" | "scheduled" | "idle";

export interface HitRegion {
  x1: number;
  x2: number;
  type: HitType;
  /** The status text (without the leading icon). */
  detail: string;
  /** Present only on idle regions with captured dumps (click target). */
  dumps: readonly TaskDump[] | null;
}

/** A waker-label hover/click region. */
export interface WakeRegion {
  x1: number;
  x2: number;
  y1: number;
  y2: number;
  wakerTaskId: number;
}

/**
 * The full per-frame render input for the task-detail canvas + interaction.
 * Draw geometry is draw-area-relative (0..drawW) - the track canvas sits AFTER
 * the shared LABEL_W DOM gutter, so no LABEL_W is added to the coordinates.
 */
export interface TaskDetailRenderModel {
  /** Task identity captured with this geometry; null for the empty model. */
  taskId: number | null;
  schedulingBands: readonly SchedulingBand[];
  /** Per-poll bars (empty when the coverage histogram is used instead). */
  pollBars: readonly PollBar[];
  /** Per-pixel coverage (opacity 0..1 per column); null in per-poll mode. */
  coverage: Float64Array | null;
  /** Count of polls overlapping the view (drives the coverage vs bars choice). */
  visiblePolls: number;
  idleBands: readonly IdleBand[];
  lifespan: LifespanBar | null;
  /** Status hit regions, in first-match-wins order. */
  hitRegions: readonly HitRegion[];
  /** Waker hover/click regions. */
  wakeRegions: readonly WakeRegion[];
}

/** Empty model (no drawW / degenerate view). */
const EMPTY_RENDER_MODEL: TaskDetailRenderModel = {
  taskId: null,
  schedulingBands: [],
  pollBars: [],
  coverage: null,
  visiblePolls: 0,
  idleBands: [],
  lifespan: null,
  hitRegions: [],
  wakeRegions: [],
};

export interface TaskDetailRenderOpts {
  data: TaskDetailData;
  viewStart: number;
  viewEnd: number;
  /** Draw-area width in CSS px (drawW from the shared layout). */
  drawW: number;
}

/**
 * First index in `polls` (sorted by start, non-overlapping) whose `end` is
 * >= viewStart - the left bound of the visible window. Binary searched so a task
 * polled millions of times never walks the whole array's head per frame.
 */
export function firstVisibleByEnd(
  polls: SpanList<PollSpan>,
  viewStart: number,
): number {
  let lo = 0;
  let hi = polls.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (polls.at(mid)!.end < viewStart) lo = mid + 1;
    else hi = mid - 1;
  }
  return lo;
}

function severityOf(delayNs: number): SchedulingSeverity {
  const delayUs = delayNs / 1000;
  if (delayUs > 1000) return "high";
  if (delayUs > 100) return "mid";
  return "low";
}

/**
 * Build the per-frame render model: spawn/wake->poll scheduling-delay bands,
 * the lifespan bar, poll bars OR the coverage histogram, and idle gaps with
 * their dump lookup.
 * Draw-area-relative coordinates. Hit regions are emitted in scheduled ->
 * polling -> idle order so the first-match-wins status lookup matches.
 */
export function buildTaskDetailRenderModel(
  opts: TaskDetailRenderOpts,
): TaskDetailRenderModel {
  const { data, viewStart, viewEnd, drawW } = opts;
  if (drawW <= 0 || viewEnd <= viewStart || !data.hasPolls) {
    return EMPTY_RENDER_MODEL;
  }
  const { polls, pollWakes } = data;
  const span = viewEnd - viewStart;
  const nsToX = (ns: number): number => ((ns - viewStart) / span) * drawW;

  const schedulingBands: SchedulingBand[] = [];
  const pollBars: PollBar[] = [];
  const idleBands: IdleBand[] = [];
  const hitRegions: HitRegion[] = [];
  const wakeRegions: WakeRegion[] = [];
  let lifespan: LifespanBar | null = null;

  // ── Spawn->first-poll delay ──────────────────────────────────────────
  const firstPoll = polls[0]!;
  if (
    data.spawnTs != null &&
    data.spawnTs < firstPoll.start &&
    firstPoll.start >= viewStart &&
    data.spawnTs <= viewEnd
  ) {
    const delay = firstPoll.start - data.spawnTs;
    const x1 = Math.max(0, nsToX(data.spawnTs));
    const x2 = Math.min(drawW, nsToX(firstPoll.start));
    const w = x2 - x1;
    if (w >= 1) {
      schedulingBands.push({
        kind: "spawn",
        x1,
        x2,
        delayNs: delay,
        severity: severityOf(delay),
        showDelayLabel: w > 25,
      });
      hitRegions.push({
        x1,
        x2,
        type: "scheduled",
        detail: `Scheduled — waiting ${formatHumanDuration(delay)} for first poll after task spawn`,
        dumps: null,
      });
    }
  }

  // ── Wake->poll delay bands + waker labels ────────────────────────────
  for (let i = 0; i < polls.length; i++) {
    const pw = pollWakes[i];
    if (pw == null) continue;
    const s = polls[i]!;
    if (s.start < viewStart || pw.effectiveWake > viewEnd) continue;
    const delay = s.start - pw.effectiveWake;
    const x1 = Math.max(0, nsToX(pw.effectiveWake));
    const x2 = Math.min(drawW, nsToX(s.start));
    const w = x2 - x1;
    if (w < 1) continue;
    const showWakerLabel = w > 40;
    schedulingBands.push({
      kind: "wake",
      x1,
      x2,
      delayNs: delay,
      severity: severityOf(delay),
      wakerTaskId: pw.wake.wakerTaskId,
      wakerLabel: pw.wakerLabel,
      showDelayLabel: w > 25,
      showWakerLabel,
    });
    hitRegions.push({
      x1,
      x2: x1 + w,
      type: "scheduled",
      detail: `Scheduled — waiting ${formatHumanDuration(delay)} for worker to poll after wake`,
      dumps: null,
    });
    if (showWakerLabel) {
      wakeRegions.push({
        x1,
        x2: x1 + w,
        y1: BAND_TOP + BAND_H,
        y2: BAND_TOP + BAND_H + 24,
        wakerTaskId: pw.wake.wakerTaskId,
      });
    }
  }

  // ── Task lifespan bar ────────────────────────────────────────────────
  if (data.spawnTs != null) {
    const lifeStart = data.spawnTs;
    const lifeEnd = data.terminateTs ?? viewEnd;
    if (lifeStart < viewEnd && lifeEnd > viewStart) {
      lifespan = {
        x1: Math.max(0, nsToX(lifeStart)),
        x2: Math.min(drawW, nsToX(lifeEnd)),
        showSpawn: lifeStart >= viewStart,
        showDone: data.terminateTs != null && lifeEnd <= viewEnd,
      };
    }
  }

  // ── Polling sections: per-poll bars, or a coverage histogram when there are
  //    more polls than pixels ─────────────────────────────────────────────
  const pollStartIdx = firstVisibleByEnd(polls, viewStart);
  let visiblePolls = 0;
  for (let i = pollStartIdx; i < polls.length; i++) {
    if (polls[i]!.start > viewEnd) break;
    visiblePolls++;
  }
  let coverage: Float64Array | null = null;
  if (visiblePolls > drawW) {
    coverage = pixelCoverage(
      polls,
      pollStartIdx,
      viewStart,
      viewEnd,
      Math.ceil(drawW),
    );
    hitRegions.push({
      x1: 0,
      x2: drawW,
      type: "polling",
      detail: `Polling — ${visiblePolls.toLocaleString()} polls in view (zoom in for per-poll detail)`,
      dumps: null,
    });
  } else {
    for (let i = pollStartIdx; i < polls.length; i++) {
      const s = polls[i]!;
      if (s.start > viewEnd) break;
      const x1 = Math.max(0, nsToX(s.start));
      const x2 = Math.min(drawW, nsToX(s.end));
      const w = Math.max(x2 - x1, 1);
      const dur = s.end - s.start;
      pollBars.push({ x1, x2: x1 + w, durationNs: dur, showLabel: w > 35 });
      hitRegions.push({
        x1,
        x2: x1 + w,
        type: "polling",
        detail: `Polling — actively executing for ${formatHumanDuration(dur)}`,
        dumps: null,
      });
    }
  }

  // ── Idle gaps between consecutive polls ──────────────────────────────
  const dumps = data.taskDumps;
  let dumpIdx = 0;
  for (let i = 0; i < polls.length - 1; i++) {
    const gapStart = polls[i]!.end;
    const gapEnd = polls[i + 1]!.start;
    if (gapEnd < viewStart || gapStart > viewEnd) continue;
    const nextPw = pollWakes[i + 1];
    const nextWakeTs = nextPw != null ? nextPw.effectiveWake : null;
    // Skip when the next poll's wake covers this gap (the wake band owns it).
    if (nextWakeTs !== null && nextWakeTs <= gapStart) continue;
    const x1 = Math.max(0, nsToX(gapStart));
    const wakeX =
      nextWakeTs !== null
        ? Math.max(0, nsToX(nextWakeTs))
        : Math.min(drawW, nsToX(gapEnd));
    const x2 = Math.min(wakeX, Math.min(drawW, nsToX(gapEnd)));
    const w = Math.max(x2 - x1, 0);
    if (w < 1) continue;

    // Dump lookup: dumps captured during poll[i-1] describe what the task waits
    // on during THIS gap. `dumps` is sorted by ts and the cursor advances
    // monotonically across the outer loop.
    const gapDumps: TaskDump[] = [];
    const prevPollStart = i > 0 ? polls[i - 1]!.start : polls[i]!.start;
    while (dumpIdx < dumps.length && dumps[dumpIdx]!.timestamp < prevPollStart) {
      dumpIdx++;
    }
    for (
      let di = dumpIdx;
      di < dumps.length && dumps[di]!.timestamp <= polls[i]!.start;
      di++
    ) {
      gapDumps.push(dumps[di]!);
    }
    const hasDump = gapDumps.length > 0;
    const dur = (nextWakeTs !== null ? nextWakeTs : gapEnd) - gapStart;
    idleBands.push({
      x1,
      x2: x1 + w,
      durationNs: dur,
      hasDump,
      dumps: hasDump ? gapDumps : [],
      showLabel: w > 35,
    });
    hitRegions.push({
      x1,
      x2: x1 + w,
      type: "idle",
      detail: hasDump
        ? `Idle — waiting ${formatHumanDuration(dur)} (click for async stack trace)`
        : `Idle — waiting ${formatHumanDuration(dur)} for waker (no wake received yet)`,
      dumps: hasDump ? gapDumps : null,
    });
  }

  return {
    taskId: data.taskId,
    schedulingBands,
    pollBars,
    coverage,
    visiblePolls,
    idleBands,
    lifespan,
    hitRegions,
    wakeRegions,
  };
}

// ── Status text (icon + detail) ───────────────────────────────────────────

/** The status icon for a hit type. */
export function hitIcon(type: HitType): string {
  if (type === "polling") return "⚡"; // lightning
  if (type === "scheduled") return "⏳"; // hourglass
  return "💤"; // zzz
}

/**
 * The status readout for a pointer at (mx, my): the first hit region containing
 * the point (first-match-wins), rendered as `<icon> <detail>`, or "" when the
 * pointer is over no region.
 */
export function statusTextAt(
  model: TaskDetailRenderModel,
  mx: number,
  my: number,
): string {
  const hit = hitRegionAt(model, mx, my);
  return hit === null ? "" : `${hitIcon(hit.type)} ${hit.detail}`;
}

/** The first status hit region under (mx, my), or null. */
export function hitRegionAt(
  model: TaskDetailRenderModel,
  mx: number,
  my: number,
): HitRegion | null {
  if (my < BAND_TOP || my > BAND_TOP + BAND_H) return null;
  for (const r of model.hitRegions) {
    if (mx >= r.x1 && mx <= r.x2) return r;
  }
  return null;
}

/** The first waker region under (mx, my), or null. */
export function wakeRegionAt(
  model: TaskDetailRenderModel,
  mx: number,
  my: number,
): WakeRegion | null {
  for (const r of model.wakeRegions) {
    if (mx >= r.x1 && mx <= r.x2 && my >= r.y1 && my <= r.y2) return r;
  }
  return null;
}

// ── Windowing descriptor ──────────────────────────────────────────────────

/**
 * The resident-data-window state the task-detail renderer must surface so a
 * segment-windowed poll list is never presented as the task's whole history.
 * Resolves to the "complete" value for a whole-trace load (the segments slice is
 * empty). Mirrors the CPU track's CpuWindow.
 */


export {
  COMPLETE_WINDOW as COMPLETE_TASK_DETAIL_WINDOW,
  type ResidentWindow as TaskDetailWindow,
} from "./resident-window.js";
