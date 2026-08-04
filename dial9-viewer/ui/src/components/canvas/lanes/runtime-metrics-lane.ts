// The per-runtime summary lane: a compact chart of one runtime's scheduler
// metrics, drawn as the group's FOOTER (directly under its worker rows, folded
// with the runtime). Shows the runtime's global-queue depth as a filled step
// area and its alive-task count as a step line on its own right-axis scale —
// the same encodings as the queue track, but scoped to one runtime and sharing
// the lanes track's time axis so it lines up with the worker lanes above.
//
// Presentation rules this lane follows so it never reads as "another worker":
//   - a distinct lighter background plus a bright top rule, and its runtime's
//     accent rail (the stripe its worker lanes carry), so the group's boundary
//     is visible and the band is legibly part of that group;
//   - a caret + "<runtime> runtime metrics" title, so it is obviously an
//     aggregate, names the runtime it summarizes, and is obviously clickable (a
//     click folds it to the one-line strip);
//   - the value of each series at the view's right edge beside its trace peak,
//     because "how deep is the queue right now" is the question the lane exists
//     to answer;
//   - a folded variant that keeps those numbers and drops only the chart.

import type { LaneDrawContext } from "./render.js";
import { RAIL_W } from "./chrome.js";
import type { RuntimeSeries } from "../../../lib/trace/runtime-metrics-model.js";
import { queueBaselineY, queueScaleY } from "../../../lib/canvas/zero-baseline.js";

const LANE_BG = "#1d2440";
const TOP_RULE = "#3d4a7a";
const GLOBAL_FILL = "rgba(79,195,247,0.30)";
const GLOBAL_STROKE = "#4fc3f7";
const TASK_STROKE = "#81c784";
const TITLE = "#aeb6e0";
const CARET = "#8b93c0";
const BASELINE_RULE = "#2b3358";
const AXIS_LABEL = "#6b73a0";
const EMPTY_TEXT = "#555";

/** Vertical margins inside the lane band: room for the title row on top and the
 *  peak-value readouts along the bottom. */
const PAD_TOP = 15;
const PAD_BOTTOM = 12;

/** What the lane needs to know about itself beyond the series. */
export interface RuntimeMetricsLaneRow {
  /** Display name of the runtime the lane summarizes ("main", "io", ...). */
  name: string;
  /** True when folded to the one-line strip (chart omitted, numbers kept). */
  collapsed: boolean;
  y: number;
  height: number;
}

/**
 * Draw one runtime's summary lane into the band at `[top, top + height)`.
 * `series` is that runtime's metric samples (undefined → an empty lane with a
 * hint). Draw-area-relative x (the canvas already omits the label gutter).
 */
export function drawRuntimeMetricsLane(
  ctx: LaneDrawContext,
  series: RuntimeSeries | undefined,
  row: RuntimeMetricsLaneRow,
  accent: string,
  viewStart: number,
  viewEnd: number,
  drawW: number,
  top: number,
): void {
  const height = row.height;
  if (drawW <= 0 || height <= 0) return;
  ctx.fillStyle = LANE_BG;
  ctx.fillRect(0, top, drawW, height);
  // The runtime's accent rail (the same stripe its worker lanes carry) plus a
  // rule across the top, marking the group boundary: everything above belongs to
  // this runtime's workers, this band is their total.
  ctx.fillStyle = accent;
  ctx.fillRect(0, top, RAIL_W, height);
  ctx.fillStyle = TOP_RULE;
  ctx.fillRect(RAIL_W, top, drawW - RAIL_W, 1);

  // Title row: caret + "<runtime> runtime metrics", so the aggregate reads as an
  // aggregate, names the runtime it summarizes, and shows the fold affordance.
  //
  // Folded, the title is DROPPED: the strip is one line, the readouts need the
  // whole width, and the label gutter already names the row. (Before the gutter
  // existed the band was the only place the name appeared, and at this title's
  // length the two overlapped.)
  const titleY = top + 11;
  drawCaret(ctx, RAIL_W + 8, titleY - 3, row.collapsed);
  if (!row.collapsed) {
    ctx.fillStyle = TITLE;
    ctx.font = "bold 10px sans-serif";
    ctx.textAlign = "left";
    ctx.fillText(metricsLaneTitle(row.name), RAIL_W + 15, titleY);
  }

  const span = viewEnd - viewStart;
  if (series === undefined || series.samples.length === 0 || span <= 0) {
    ctx.fillStyle = EMPTY_TEXT;
    ctx.font = "10px sans-serif";
    ctx.textAlign = "right";
    ctx.fillText("no samples", Math.max(0, drawW - 4), titleY);
    return;
  }

  // The series' level at the right edge of the view: the "right now" reading.
  const atEdge = levelAt(series, viewEnd);

  // The one-line strip: keep the numbers, drop the chart. Drawn on the title row
  // (whose title the fold omits) so a folded lane costs one line of height.
  if (row.collapsed) {
    ctx.font = "9px monospace";
    ctx.fillStyle = GLOBAL_STROKE;
    ctx.textAlign = "left";
    ctx.fillText(
      queueLabel(atEdge.globalQueue, series.maxGlobalQueue),
      RAIL_W + 15,
      titleY,
    );
    ctx.fillStyle = TASK_STROKE;
    ctx.textAlign = "right";
    ctx.fillText(taskLabel(atEdge.aliveTasks, series.maxAliveTasks), Math.max(0, drawW - 4), titleY);
    return;
  }

  const chartTop = top + PAD_TOP;
  const chartH = height - PAD_TOP - PAD_BOTTOM;
  if (chartH <= 0) return;
  // The EXPLICIT zero baseline (shared with the queue track, issue #282): a
  // runtime whose global queue never backs up is the common healthy case, and at
  // a true-bottom baseline that flat zero fused to the axis and read as "no
  // data". Here it reads as a flat line sitting on a visible floor.
  const baseY = queueBaselineY(chartTop, chartH);
  const xOf = (t: number): number => ((t - viewStart) / span) * drawW;

  const inView = series.samples.filter((s) => s.t >= viewStart && s.t <= viewEnd);
  // Seed from the last sample before the window so the step is correct at x=0.
  let seedIdx = -1;
  for (let i = 0; i < series.samples.length; i++) {
    if (series.samples[i]!.t < viewStart) seedIdx = i;
    else break;
  }
  const seed = seedIdx >= 0 ? series.samples[seedIdx]! : (inView[0] ?? null);
  if (seed === null) return;

  // The zero floor, drawn under both series so a flat-zero queue is visibly ON a
  // baseline rather than ambiguously absent, plus a "0" tick so the floor is
  // labelled and the chart's vertical extent is legible without the readouts.
  ctx.fillStyle = BASELINE_RULE;
  ctx.fillRect(RAIL_W, baseY, drawW - RAIL_W, 1);
  ctx.fillStyle = AXIS_LABEL;
  ctx.font = "8px monospace";
  ctx.textAlign = "left";
  ctx.fillText("0", RAIL_W + 2, baseY - 1);

  // ── Global queue: filled step area (own magnitude) ──────────────────────
  const maxQ = Math.max(1, series.maxGlobalQueue);
  const qy = (v: number): number => queueScaleY(v, maxQ, chartTop, chartH);

  ctx.beginPath();
  ctx.moveTo(0, baseY);
  let qLast = seed.globalQueue;
  ctx.lineTo(0, qy(qLast));
  for (const s of inView) {
    const x = xOf(s.t);
    ctx.lineTo(x, qy(qLast)); // hold previous level
    qLast = s.globalQueue;
    ctx.lineTo(x, qy(qLast)); // step to new level
  }
  ctx.lineTo(drawW, qy(qLast));
  ctx.lineTo(drawW, baseY);
  ctx.closePath();
  ctx.fillStyle = GLOBAL_FILL;
  ctx.fill();
  ctx.strokeStyle = GLOBAL_STROKE;
  ctx.lineWidth = 1;
  ctx.beginPath();
  qLast = seed.globalQueue;
  ctx.moveTo(0, qy(qLast));
  for (const s of inView) {
    const x = xOf(s.t);
    ctx.lineTo(x, qy(qLast));
    qLast = s.globalQueue;
    ctx.lineTo(x, qy(qLast));
  }
  ctx.lineTo(drawW, qy(qLast));
  ctx.stroke();

  // ── Alive tasks: step line on its own scale ─────────────────────────────
  const maxT = Math.max(1, series.maxAliveTasks);
  const ty = (v: number): number => queueScaleY(v, maxT, chartTop, chartH);
  ctx.strokeStyle = TASK_STROKE;
  ctx.lineWidth = 1.25;
  ctx.beginPath();
  let tLast = seed.aliveTasks;
  ctx.moveTo(0, ty(tLast));
  for (const s of inView) {
    const x = xOf(s.t);
    ctx.lineTo(x, ty(tLast));
    tLast = s.aliveTasks;
    ctx.lineTo(x, ty(tLast));
  }
  ctx.lineTo(drawW, ty(tLast));
  ctx.stroke();

  // ── Readouts: one label per series, colour-keyed to its stroke ───────────
  // Current value first (the number a backlog investigation wants), then the
  // peak. ONE fillText per side, so nothing depends on measuring text width.
  const baselineY = top + height - 3;
  ctx.font = "9px monospace";
  ctx.fillStyle = GLOBAL_STROKE;
  ctx.textAlign = "left";
  ctx.fillText(queueLabel(atEdge.globalQueue, series.maxGlobalQueue), RAIL_W + 4, baselineY);
  ctx.fillStyle = TASK_STROKE;
  ctx.textAlign = "right";
  ctx.fillText(taskLabel(atEdge.aliveTasks, series.maxAliveTasks), Math.max(0, drawW - 4), baselineY);
}

// The readouts pair a POINT reading with the trace peak. "at <right edge of the
// view>" is spelled out because the two can disagree by design: a final
// shutdown sample lands in the last sub-pixel column, so the chart can look
// high while the edge reading is 0. Naming the reading's position makes that
// readable rather than contradictory. The peak is the trace peak, which is also
// what the chart's y scale uses — so a quiet window plots flat against an
// off-screen peak, and the label says what that peak is.

/**
 * The summary lane's title: "<runtime> runtime metrics". Exported so the label
 * gutter and the tests name the row the same way the band does.
 */
export function metricsLaneTitle(runtimeName: string): string {
  return `${runtimeName} runtime metrics`;
}

/** The global-queue readout: depth at the view's right edge, then the trace peak. */
function queueLabel(current: number, max: number): string {
  return `global queue: ${current} at view end · peak ${max}`;
}

/** The alive-task readout: count at the view's right edge, then the trace peak. */
function taskLabel(current: number, max: number): string {
  return `alive tasks: ${current} at view end · peak ${max}`;
}

/**
 * The series' level at time `t`: the last sample at or before `t`, or the first
 * sample when `t` precedes the series. Step semantics, matching the draw. Never
 * invents a zero for a gap — an empty series has no level, so callers must have
 * checked for samples first.
 */
function levelAt(
  series: RuntimeSeries,
  t: number,
): { globalQueue: number; aliveTasks: number } {
  const samples = series.samples;
  let lo = 0;
  let hi = samples.length - 1;
  let ans = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (samples[mid]!.t <= t) {
      ans = mid;
      lo = mid + 1;
    } else hi = mid - 1;
  }
  const s = samples[ans >= 0 ? ans : 0]!;
  return { globalQueue: s.globalQueue, aliveTasks: s.aliveTasks };
}

/** A small disclosure caret centred at (cx, cy): right-pointing when collapsed,
 *  down-pointing when expanded. Mirrors the runtime header's caret. */
function drawCaret(ctx: LaneDrawContext, cx: number, cy: number, collapsed: boolean): void {
  ctx.fillStyle = CARET;
  ctx.beginPath();
  if (collapsed) {
    ctx.moveTo(cx - 3, cy - 4);
    ctx.lineTo(cx + 3, cy);
    ctx.lineTo(cx - 3, cy + 4);
  } else {
    ctx.moveTo(cx - 4, cy - 3);
    ctx.lineTo(cx + 4, cy - 3);
    ctx.lineTo(cx, cy + 3);
  }
  ctx.closePath();
  ctx.fill();
}
