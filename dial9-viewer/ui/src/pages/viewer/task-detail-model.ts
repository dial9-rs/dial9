// src/pages/viewer/task-detail-model.ts - the task-detail track's PURE
// derivation + per-frame render model (T30; features/02 section N; legacy
// renderTaskDetail in viewer.html:4358-4713). No store, no DOM, no canvas:
// every function is referentially transparent so the track component
// (task-detail-track.ts) can cache the SELECTION-keyed collection in a store
// `derived()` (perf finding F5 - renderTaskDetail re-collected+sorted every
// poll of the task on EVERY frame) and unit-test the numbers directly (the
// DoD's "task numbers exact vs legacy" check).
//
// The split mirrors F5's two tiers + the hybrid track/inspector decision:
//   1. computeTaskDetailData(source, trace, taskId) - SELECTION-invariant
//      given the selected task: collect the task's polls across all workers
//      (sorted by start), its wakes, the per-poll wake match (computePollWakes,
//      O(P logP)), the lifetime/spawn/terminate numbers (N2), the waker labels
//      (N7), and the task-dump references (N4/N11). This is BOTH the timeline
//      track's input AND the data T31's inspector Task tab renders (the hybrid
//      split: this file owns the derivation, T31 owns the textual tab). Cached
//      in a store `derived(["trace","selection"], ...)` - selection change
//      invalidates, PAN does not (the explicit DoD).
//   2. buildTaskDetailRenderModel(...) - VIEWPORT-dependent: the visible poll
//      window, wake->poll delay bands (N6), poll bars / coverage histogram
//      (N10), idle gaps (N11), the lifespan bar (N9), and the hit/wake regions
//      the interaction layer reads (N5 status, N8 waker hover/click). Pure and
//      cheap enough to memoize on its primitive inputs so a hover-only redraw
//      (the G8 waker highlight) never re-walks the collection.
//
// T17 obligation (audit notes 6+7): a segment-windowed collection can be
// missing the task's polls beyond the resident window. The render carries a
// TaskDetailWindow descriptor and surfaces a truncated/oversized window rather
// than presenting a clipped poll list as the task's whole history.

import {
  EVENT_TYPES,
  buildWorkerSpans,
  computePollWakes,
  computeRuntimeGroups,
  formatHumanDuration,
} from "../../lib/trace/index.js";
import type {
  ParsedTrace,
  PollSpan,
  TaskDump,
  TaskWake,
  WorkerLane,
} from "../../lib/trace/index.js";
import { pixelCoverage } from "../../lib/canvas/index.js";

// ── Trace-invariant poll source (buildWorkerSpans, keyed by trace) ────────

/**
 * The per-worker poll lanes + wake index the task-detail collection reads,
 * derived once per trace (buildWorkerSpans scans every event). The component
 * wraps this in a store `derived(["trace"], ...)` so it is NOT rebuilt when
 * the selected task or the viewport changes (F5).
 */
export interface TaskPollSource {
  /** Distinct runtime worker ids (render order irrelevant here). */
  workerIds: readonly number[];
  /** Reconstructed poll/park/active spans per worker. */
  workerSpans: Record<number, WorkerLane>;
  /** Wake events indexed by woken task (legacy wakesByTask). */
  wakesByTask: Record<number, TaskWake[]>;
}

/** Empty source (no trace) so callers never special-case null. */
export const EMPTY_TASK_POLL_SOURCE: TaskPollSource = {
  workerIds: [],
  workerSpans: {},
  wakesByTask: {},
};

/**
 * The distinct worker ids in a trace, in runtime-group order (mirrors the
 * lanes' deriveWorkerIds without coupling to that module): scan non-queue,
 * non-wake events for the worker set, then reorder by runtime groups. Only
 * the SET and its COUNT matter for task-detail (poll collection + the "io"
 * waker discriminant, legacy line 4534), but the grouped order is kept so the
 * count matches the legacy `workerIds.length` exactly.
 */
export function collectWorkerIds(trace: ParsedTrace): number[] {
  const set = new Set<number>();
  for (const e of trace.events) {
    if (
      e.eventType === EVENT_TYPES.QueueSample ||
      e.eventType === EVENT_TYPES.WakeEvent
    ) {
      continue;
    }
    set.add(e.workerId);
  }
  const sorted = [...set].sort((a, b) => a - b);
  return computeRuntimeGroups(sorted, trace.runtimeWorkers).flatMap(
    (g) => g.workerIds,
  );
}

/**
 * Build the trace-invariant poll source (buildWorkerSpans + wake index).
 * Returns EMPTY_TASK_POLL_SOURCE for a null/eventless trace.
 */
export function buildTaskPollSource(trace: ParsedTrace | null): TaskPollSource {
  if (trace === null || trace.maxTs === null) return EMPTY_TASK_POLL_SOURCE;
  const workerIds = collectWorkerIds(trace);
  const result = buildWorkerSpans(
    trace.events,
    workerIds,
    trace.maxTs,
    trace.blockInPlaceGaps,
  );
  return {
    workerIds,
    workerSpans: result.workerSpans,
    wakesByTask: result.wakesByTask,
  };
}

// ── Per-task detail data (selection-keyed; N2/N4/N6-N9) ───────────────────

/**
 * One poll's matched wake (legacy computePollWakes entry) plus the resolved
 * waker LABEL (N7), precomputed here because it depends only on the trace +
 * wake (viewport-invariant) - so the per-frame render model never re-resolves
 * spawn locations while panning.
 */
export interface PollWakeInfo {
  wake: TaskWake;
  /** The wake time bumped past an earlier poll that contained it. */
  effectiveWake: number;
  /** N7 label: "io" for runtime/worker wakes, else spawn filename / hex id. */
  wakerLabel: string;
}

/**
 * Everything about ONE selected task that does NOT depend on the viewport -
 * the timeline track's input AND the data T31's inspector Task tab renders
 * (the hybrid split). Cached in a store `derived(["trace","selection"], ...)`.
 */
export interface TaskDetailData {
  /** The selected task, or null when nothing is selected. */
  taskId: number | null;
  /** All polls for the task across every worker, sorted by start (N10). */
  polls: readonly PollSpan[];
  /** The task's wake events (N2 wake count source). */
  wakes: readonly TaskWake[];
  /** Per-poll wake match + resolved label, parallel to `polls` (N6/N7). */
  pollWakes: readonly (PollWakeInfo | null)[];
  /** Poll count (N2). */
  pollCount: number;
  /** Wake count (N2; shown only when instrumented). */
  wakeCount: number;
  /** Resolved spawn location string for the task, or null (N2). */
  spawnLocation: string | null;
  /** False for tasks spawned via raw tokio::spawn (N3 badge). */
  isInstrumented: boolean;
  /** Spawn timestamp (ns), or null when untracked (N9). */
  spawnTs: number | null;
  /** Terminate timestamp (ns), or null when the task did not finish (N9). */
  terminateTs: number | null;
  /** True when a terminate time exists (the N2 completion mark). */
  hasTerminate: boolean;
  /** terminate - spawn (ns), or null when either is missing (N2 lifetime). */
  lifetimeNs: number | null;
  /** Async-stack captures for the task, sorted by ts (N4 count, N11 stacks). */
  taskDumps: readonly TaskDump[];
  /** Distinct worker count (the "io" waker discriminant, N7). */
  workerIdCount: number;
  /** True once the task has >= 1 poll - the N1 track-visibility predicate. */
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
 * Resolve the waker label for a wake's `wakerTaskId` (legacy renderTaskDetail
 * viewer.html:4531-4540): runtime/worker wakes (falsy, 0, or in the worker-id
 * range) show "io"; otherwise the waker task's spawn filename, falling back to
 * its hex id.
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
 * Collect the selection-keyed detail for `taskId`. Ports the legacy
 * renderTaskDetail collection (viewer.html:4366-4411) EXACTLY: gather the
 * task's polls across every worker and sort by start; match wakes with the
 * frozen-core computePollWakes; read the N2 numbers from the trace's task
 * maps. Returns EMPTY_TASK_DETAIL_DATA when nothing is selected or the task
 * has no polls (the N1 "hide on deselect / no polls" contract).
 */
export function computeTaskDetailData(
  source: TaskPollSource,
  trace: ParsedTrace | null,
  taskId: number | null,
): TaskDetailData {
  if (taskId === null || trace === null) return EMPTY_TASK_DETAIL_DATA;

  // Collect all polls for this task across all workers, sorted by start
  // (legacy 4367-4373).
  const polls: PollSpan[] = [];
  for (const w of source.workerIds) {
    const lane = source.workerSpans[w];
    if (lane === undefined) continue;
    for (const s of lane.polls) {
      if (s.taskId === taskId) polls.push(s);
    }
  }
  polls.sort((a, b) => a.start - b.start);
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
  const spawnLocId = firstPoll.spawnLocId;
  const spawnLocation =
    spawnLocId != null ? trace.spawnLocations.get(spawnLocId) ?? null : null;

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
 * The N2 label as PLAIN text (task id, spawn location, poll count, wake count
 * when instrumented, lifetime, completion mark). Ported from the legacy
 * labelHtml assembly (viewer.html:4397-4403) MINUS the HTML badges/links (N3
 * badge and N4 flamegraph link are T31's inspector surface). The numbers here
 * are the DoD's exact behavioral-diff target; T31 renders the rich version
 * from the same TaskDetailData.
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

// ── Per-frame render model (viewport-dependent; N6-N12) ───────────────────

/** Band vertical placement (legacy bandTop / bandH constants). */
export const BAND_TOP = 50;
export const BAND_H = 30;

/** Delay severity (legacy delayUs thresholds): green / orange / red. */
export type WakeSeverity = "low" | "mid" | "high";

/** A wake->poll scheduling-delay band (N6) + its waker label (N7). */
export interface WakeBand {
  /** Draw-area-relative x of the effective-wake edge (px). */
  x1: number;
  /** Draw-area-relative x of the poll-start edge (px). */
  x2: number;
  delayNs: number;
  severity: WakeSeverity;
  wakerTaskId: number;
  wakerLabel: string;
  /** N6 duration label shown only when the band is wider than 25px. */
  showDelayLabel: boolean;
  /** N7 waker label shown (and clickable) only when wider than 40px. */
  showWakerLabel: boolean;
}

/** A cyan poll bar (N10; per-poll regime). */
export interface PollBar {
  x1: number;
  x2: number;
  durationNs: number;
  /** Per-poll duration label shown only when wider than 35px. */
  showLabel: boolean;
}

/** An idle gap between consecutive polls (N11). */
export interface IdleBand {
  x1: number;
  x2: number;
  durationNs: number;
  /** True when a task dump was captured for this gap (cross-hatch + purple). */
  hasDump: boolean;
  /** The captured dumps (N11 stack; T31/T32 build the flamegraph from them). */
  dumps: readonly TaskDump[];
  /** Duration label shown only when wider than 35px. */
  showLabel: boolean;
}

/** The task lifespan bar (N9), spawn -> terminate. */
export interface LifespanBar {
  x1: number;
  x2: number;
  /** Draw the spawn edge marker (spawn is inside the view). */
  showSpawn: boolean;
  /** Draw the done edge marker (terminate is inside the view). */
  showDone: boolean;
}

/** A hover status hit region (N5): polling / scheduled / idle. */
export type HitType = "polling" | "scheduled" | "idle";

export interface HitRegion {
  x1: number;
  x2: number;
  type: HitType;
  /** The N5 status text (without the leading icon). */
  detail: string;
  /** Present only on idle regions with captured dumps (N11 click target). */
  dumps: readonly TaskDump[] | null;
}

/** A waker-label hover/click region (N8). */
export interface WakeRegion {
  x1: number;
  x2: number;
  y1: number;
  y2: number;
  wakerTaskId: number;
}

/**
 * The full per-frame render input for the task-detail canvas + interaction.
 * Draw geometry is draw-area-relative (0..drawW) - the track canvas sits
 * AFTER the shared LABEL_W DOM gutter, so unlike legacy no LABEL_W is added
 * (the A13 alignment shift every migrated track makes).
 */
export interface TaskDetailRenderModel {
  wakeBands: readonly WakeBand[];
  /** Per-poll bars (empty when the coverage histogram is used instead). */
  pollBars: readonly PollBar[];
  /** N10 per-pixel coverage (opacity 0..1 per column); null in per-poll mode. */
  coverage: Float64Array | null;
  /** Count of polls overlapping the view (drives the coverage vs bars choice). */
  visiblePolls: number;
  idleBands: readonly IdleBand[];
  lifespan: LifespanBar | null;
  /** Status hit regions, in the legacy first-match-wins order. */
  hitRegions: readonly HitRegion[];
  /** Waker hover/click regions (N8). */
  wakeRegions: readonly WakeRegion[];
}

/** Empty model (no drawW / degenerate view). */
const EMPTY_RENDER_MODEL: TaskDetailRenderModel = {
  wakeBands: [],
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
 * >= viewStart - the left bound of the visible window (legacy findFirstVisible,
 * viewer.html:2704-2716). Binary searched so a task polled millions of times
 * never walks the whole array's head per frame.
 */
export function firstVisibleByEnd(
  polls: readonly PollSpan[],
  viewStart: number,
): number {
  let lo = 0;
  let hi = polls.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (polls[mid]!.end < viewStart) lo = mid + 1;
    else hi = mid - 1;
  }
  return lo;
}

function severityOf(delayNs: number): WakeSeverity {
  const delayUs = delayNs / 1000;
  if (delayUs > 1000) return "high";
  if (delayUs > 100) return "mid";
  return "low";
}

/**
 * Build the per-frame render model, porting renderTaskDetail's geometry
 * (viewer.html:4466-4704) into pure data: wake->poll delay bands (N6/N7), the
 * lifespan bar (N9), poll bars OR the coverage histogram (N10), and idle gaps
 * with their dump lookup (N11). Draw-area-relative coordinates. Hit regions
 * are emitted in the legacy scheduled -> polling -> idle order so the
 * first-match-wins status lookup (N5) matches exactly.
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

  const wakeBands: WakeBand[] = [];
  const pollBars: PollBar[] = [];
  const idleBands: IdleBand[] = [];
  const hitRegions: HitRegion[] = [];
  const wakeRegions: WakeRegion[] = [];
  let lifespan: LifespanBar | null = null;

  // ── Wake->poll delay bands (N6) + waker labels (N7) ──────────────────
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
    wakeBands.push({
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

  // ── Task lifespan bar (N9) ───────────────────────────────────────────
  if (data.hasTerminate && data.spawnTs != null && data.terminateTs != null) {
    const lifeStart = data.spawnTs;
    const lifeEnd = data.terminateTs;
    if (lifeStart < viewEnd && lifeEnd > viewStart) {
      lifespan = {
        x1: Math.max(0, nsToX(lifeStart)),
        x2: Math.min(drawW, nsToX(lifeEnd)),
        showSpawn: lifeStart >= viewStart,
        showDone: lifeEnd <= viewEnd,
      };
    }
  }

  // ── Polling sections (N10): per-poll bars, or a coverage histogram when
  //    there are more polls than pixels (legacy 4590-4644) ───────────────
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

  // ── Idle gaps between consecutive polls (N11) ────────────────────────
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

    // Dump lookup (legacy 4664-4675): dumps captured during poll[i-1] describe
    // what the task waits on during THIS gap. `dumps` is sorted by ts and the
    // cursor advances monotonically across the outer loop.
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
    wakeBands,
    pollBars,
    coverage,
    visiblePolls,
    idleBands,
    lifespan,
    hitRegions,
    wakeRegions,
  };
}

// ── N5 status text (icon + detail) ────────────────────────────────────────

/** The N5 status icon for a hit type (legacy line 6605). */
export function hitIcon(type: HitType): string {
  if (type === "polling") return "⚡"; // lightning
  if (type === "scheduled") return "⏳"; // hourglass
  return "💤"; // zzz
}

/**
 * The N5 status readout for a pointer at (mx, my): the first hit region
 * containing the point (first-match-wins, legacy 6596-6606), rendered as
 * `<icon> <detail>`, or "" when the pointer is over no region.
 */
export function statusTextAt(
  model: TaskDetailRenderModel,
  mx: number,
  my: number,
): string {
  const hit = hitRegionAt(model, mx, my);
  return hit === null ? "" : `${hitIcon(hit.type)} ${hit.detail}`;
}

/** The first status hit region under (mx, my), or null (N5). */
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

/** The first waker region under (mx, my), or null (N8). */
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

// ── T17 windowing descriptor (carried obligation, audit notes 6+7) ────────

/**
 * The resident-data-window state the task-detail renderer must surface so a
 * segment-windowed poll list is never presented as the task's whole history.
 * Resolves to the "complete" value for a whole-trace load (the shell's current
 * path; the segments slice is empty). Mirrors the CPU track's CpuWindow.
 */
export interface TaskDetailWindow {
  /** Which edge(s) of the resident window truncate the poll data. */
  truncatedAt: "start" | "end" | "both" | null;
  /** True when a needed segment is terminally "oversized" (never resident). */
  oversized: boolean;
}

/** The "complete window" descriptor (whole-trace load, no windowing). */
export const COMPLETE_TASK_DETAIL_WINDOW: TaskDetailWindow = {
  truncatedAt: null,
  oversized: false,
};
