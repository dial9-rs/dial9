// The pinned per-runtime summary lane: a compact chart of one runtime's
// scheduler metrics, drawn as the group's first lane (above its workers, folded
// with the runtime). Shows the runtime's global-queue depth as a filled step
// area and its alive-task count as a step line on its own right-axis scale —
// the same encodings as the queue track, but scoped to one runtime and sharing
// the lanes track's time axis so it lines up with the worker lanes below.

import type { LaneDrawContext } from "./render.js";
import type { RuntimeSeries } from "../../../lib/trace/runtime-metrics-model.js";

const LANE_BG = "#141a2e";
const GLOBAL_FILL = "rgba(79,195,247,0.28)";
const GLOBAL_STROKE = "#4fc3f7";
const TASK_STROKE = "#81c784";
const LABEL = "#6b73a0";
const TASK_LABEL = "#81c784";
const EMPTY_TEXT = "#555";

// Vertical margins inside the lane band.
const PAD_TOP = 12;
const PAD_BOTTOM = 4;

/**
 * Draw one runtime's summary lane into the band at `[top, top + height)`.
 * `series` is that runtime's metric samples (undefined → an empty lane with a
 * hint). Draw-area-relative x (the canvas already omits the label gutter).
 */
export function drawRuntimeMetricsLane(
  ctx: LaneDrawContext,
  series: RuntimeSeries | undefined,
  viewStart: number,
  viewEnd: number,
  drawW: number,
  top: number,
  height: number,
): void {
  ctx.fillStyle = LANE_BG;
  ctx.fillRect(0, top, drawW, height);
  if (drawW <= 0 || height <= 0) return;

  ctx.fillStyle = LABEL;
  ctx.font = "10px sans-serif";
  ctx.textAlign = "left";
  ctx.fillText("runtime metrics", 4, top + 10);

  const span = viewEnd - viewStart;
  if (series === undefined || series.samples.length === 0 || span <= 0) {
    ctx.fillStyle = EMPTY_TEXT;
    ctx.font = "10px sans-serif";
    ctx.fillText("no samples", 4, top + height - 6);
    return;
  }

  const chartTop = top + PAD_TOP;
  const chartH = height - PAD_TOP - PAD_BOTTOM;
  if (chartH <= 0) return;
  const baseY = chartTop + chartH;
  const xOf = (t: number): number => ((t - viewStart) / span) * drawW;

  // ── Global queue: filled step area (own magnitude) ──────────────────────
  const maxQ = Math.max(1, series.maxGlobalQueue);
  const qy = (v: number): number => baseY - (Math.min(v, maxQ) / maxQ) * chartH;
  const inView = series.samples.filter((s) => s.t >= viewStart && s.t <= viewEnd);
  // Seed from the last sample before the window so the step is correct at x=0.
  let seedIdx = -1;
  for (let i = 0; i < series.samples.length; i++) {
    if (series.samples[i]!.t < viewStart) seedIdx = i;
    else break;
  }
  const seed = seedIdx >= 0 ? series.samples[seedIdx]! : (inView[0] ?? null);
  if (seed === null) return;

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

  // ── Active tasks: step line on its own scale ────────────────────────────
  const maxT = Math.max(1, series.maxAliveTasks);
  const ty = (v: number): number => baseY - (Math.min(v, maxT) / maxT) * chartH;
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

  // ── Labels: peak queue (left) + peak tasks (right) ──────────────────────
  ctx.fillStyle = GLOBAL_STROKE;
  ctx.font = "9px monospace";
  ctx.textAlign = "left";
  ctx.fillText(`queue max ${series.maxGlobalQueue}`, 4, top + height - 4);
  ctx.fillStyle = TASK_LABEL;
  ctx.textAlign = "right";
  ctx.fillText(`tasks max ${series.maxAliveTasks}`, Math.max(0, drawW - 4), top + height - 4);
}
