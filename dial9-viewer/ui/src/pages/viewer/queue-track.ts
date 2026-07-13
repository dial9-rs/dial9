// src/pages/viewer/queue-track.ts - the Queue depth track component (T29;
// features/02 section M). Hosts inside T21's unified track column: tracks.ts
// delegates the "queue" row's TEMPLATE (label + legend + canvas) and its canvas
// PAINT here. Everything reactive flows through the store (T07); the heavy
// series derivation is a store.derived cache over the `trace` slice (F5) and the
// per-frame render model is memoized on its primitive inputs.
//
// S6 (the core correctness ask, issue #282): the global + max-local + active-task
// series render against ONE explicit zero baseline so a 0-valued global queue
// shows a VISIBLE flat line instead of a stroke fused to the axis. The scale is
// queue-model.ts `queueScaleY`; the numbers are unchanged from legacy (J7), only
// the y-mapping and the labeling/legend (F3) differ - the ledgered presentation
// delta.
//
// SEAMS:
//  - M6 hover info rides T24's at-cursor store contract, NOT a corner div. T24's
//    overlay listens for mousemove on the WHOLE track column (components/overlay/
//    index.ts) and writes Global Q / Local max / Active tasks to
//    `transient.atCursor` for the ns under the cursor - so hovering THIS track
//    already populates the inspector readout (T31 renders it). No separate hover
//    dispatch is needed here; that is the point of the shared contract.
//  - M7 drag-select dispatches the selected range to `selection.spawnedTasksRange`;
//    T31's inspector renders the "tasks spawned in range" list (queue-model.ts
//    `computeSpawnedTasks` is the shared derivation). M8 (clicking a listed
//    task-id link -> selectedTaskId) is T31's link handler over that list. Both
//    RENDER halves are deferred-until-T31 (see HANDOFF); T29 owns the dispatch.
//  - The in-lane "q:NN" local-queue legend entry lives in T22's merged lanes
//    legend (components/canvas/lanes/legend.ts, "local queue (q:NN)"); THIS
//    track's legend covers ITS OWN encodings (F3): global area, max-local line,
//    active-task line.
//
// T17 (audit notes 6+7): a segment-windowed view can be missing samples beyond
// the resident window; drawWindowMarkers surfaces a truncated/oversized window
// rather than painting a clipped chart as complete (mirrors cpu.ts).

import { html, type TemplateResult } from "lit-html";
import { repeat } from "lit-html/directives/repeat.js";
import { createCanvasSizer } from "../../lib/canvas/index.js";
import type { CanvasSizer } from "../../lib/canvas/index.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import type { TimeRange } from "../../types/trace.js";
import type { TrackSpec } from "./track-layout.js";
import {
  COMPLETE_QUEUE_WINDOW,
  QUEUE_LEGEND,
  buildQueueRenderModel,
  computeQueueData,
  computeSpawnedTasks,
  deriveQueueWindow,
  queueBaselineY,
  queueScaleY,
  type QueueData,
  type QueueRenderModel,
  type QueueWindow,
  type SpawnedTasksResult,
} from "./queue-model.js";

/** Height reserved for the legend strip above the queue canvas. */
const LEGEND_H = 18;

// Legacy queue-panel colours (viewer.html), kept identical so the migrated
// track reads the same.
const CANVAS_BG = "#16213e";
const GLOBAL_FILL = "rgba(79,195,247,0.3)";
const GLOBAL_STROKE = "#4fc3f7";
const LOCAL_STROKE = "#ff8a65";
const ACTIVE_STROKE = "#81c784";
const AXIS_LABEL = "#666";
const ACTIVE_LABEL = "#81c784";
const EMPTY_TEXT = "#555";
const SEL_FILL = "rgba(129,199,132,0.2)";
const SEL_STROKE = "rgba(129,199,132,0.6)";
const TRUNC_BAND = "rgba(255,120,120,0.28)";
const TRUNC_LABEL = "#ffb3b3";

// Vertical chart margins inside the canvas (below the legend strip). Headroom
// at top for the maxQ label, a little at the bottom for the "0" label; the
// explicit zero baseline (queueScaleY) sits just above CHART_BOTTOM.
const CHART_TOP = 12;
const CHART_BOTTOM = 6;

/** The handle tracks.ts + the shell drive. */
export interface QueueTrackController {
  /** The full `.d9-track` row template for the queue track (M4/M5 + legend). */
  rowTemplate(track: TrackSpec): TemplateResult;
  /** Size + draw the queue canvas (M1/M2/M3). Called from sizeTracks inside
   *  the shell's frame tick. */
  paint(
    canvas: HTMLCanvasElement,
    drawW: number,
    trackHeight: number,
    dpr: number,
    viewStart: number,
    viewEnd: number,
  ): void;
  /** Read the current (cached) queue data - T31's spawned-tasks source. */
  data(): QueueData;
  /**
   * The M7 spawned-tasks derivation for a range (T31 renders it): tasks first
   * polled in range, grouped by spawn location. Exposed so T31 reuses the
   * logic and so the dispatch is testable without a canvas.
   */
  spawnedTasks(range: TimeRange): SpawnedTasksResult | null;
  /**
   * M7 dispatch: write the drag-selected range to `selection.spawnedTasksRange`
   * (null clears it). DOM-free so the dispatch is Vitest-testable. Only writes
   * on an actual change (no store thrash).
   */
  commitRange(range: TimeRange | null): void;
  /** Tear down (drag listeners, sizer). */
  dispose(): void;
}

export function createQueueTrack(store: ViewerStore): QueueTrackController {
  // Trace-invariant queue series: recomputed only when the trace slice changes.
  const queueData = store.derived(["trace"], (s) => computeQueueData(s.trace.trace));

  // Per-frame render-model memo, keyed on the derived-data reference (trace
  // identity) + the primitive viewport/width inputs, so a selection-only change
  // (an M7 range dispatch) reuses the cached buckets instead of rebuilding them.
  let modelCache: { data: QueueData; key: string; model: QueueRenderModel } | null = null;
  let sizer: CanvasSizer<CanvasRenderingContext2D> | null = null;
  let sizerCanvas: HTMLCanvasElement | null = null;

  // The geometry of the last paint, so the drag handler can repaint the chart +
  // selection band without going through the store (legacy drew the band
  // directly on the queue canvas per mousemove).
  let lastPaint: {
    canvas: HTMLCanvasElement;
    drawW: number;
    canvasH: number;
    dpr: number;
    viewStart: number;
    viewEnd: number;
    model: QueueRenderModel;
    window: QueueWindow;
  } | null = null;

  // Drag-select state (self-contained, like legacy qcDragging locals). The
  // gesture is queue-track-local; it does NOT set transient.drag (that slice is
  // T23's lane pan/region/zoom).
  let drag: { startX: number; startNs: number; endNs: number; moved: boolean } | null =
    null;

  function state(): StoreState {
    return store.getState() as StoreState;
  }

  function renderModel(
    data: QueueData,
    viewStart: number,
    viewEnd: number,
    drawW: number,
  ): QueueRenderModel {
    const key = `${viewStart}|${viewEnd}|${drawW}`;
    if (modelCache !== null && modelCache.data === data && modelCache.key === key) {
      return modelCache.model;
    }
    const model = buildQueueRenderModel({ data, viewStart, viewEnd, drawW });
    modelCache = { data, key, model };
    return model;
  }

  // ── Canvas paint (M1/M2/M3) ────────────────────────────────────────────

  function paint(
    canvas: HTMLCanvasElement,
    drawW: number,
    trackHeight: number,
    dpr: number,
    viewStart: number,
    viewEnd: number,
  ): void {
    if (sizer === null || sizerCanvas !== canvas) {
      sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      sizerCanvas = canvas;
    }
    const canvasH = Math.max(0, trackHeight - LEGEND_H);
    const ctx = sizer.ensure(drawW, canvasH, dpr);
    const s = state();
    const data = queueData();
    const model = renderModel(data, viewStart, viewEnd, drawW);
    const window = deriveQueueWindow(s);
    drawQueueCanvas(ctx, model, window, drawW, canvasH);
    lastPaint = { canvas, drawW, canvasH, dpr, viewStart, viewEnd, model, window };
    // If a drag is in flight (a store tick landed mid-drag), re-overlay the band.
    if (drag !== null && drag.moved) overlayBand(ctx, drawW, canvasH, viewStart, viewEnd);
  }

  // ── Templates (M4 label, M5 legend) ────────────────────────────────────

  function rowTemplate(track: TrackSpec): TemplateResult {
    return html`
      <div
        class="d9-track d9-track--queue"
        data-track-id=${track.id}
        style="height:${track.height}px"
      >
        <div class="d9-track-label" id="d9-track-label-queue">
          <span class="d9-track-name">${track.label}</span>
        </div>
        <div class="d9-track-canvas-wrap d9-queue-wrap">
          ${legendTemplate()}
          <canvas
            class="d9-track-canvas d9-queue-canvas"
            data-track-canvas=${track.id}
            aria-labelledby="d9-track-label-queue"
            role="img"
            @mousedown=${onCanvasMouseDown}
          ></canvas>
        </div>
      </div>
    `;
  }

  /** M5 + F3: the in-track legend, swatch encodings matching the draw. */
  function legendTemplate(): TemplateResult {
    return html`
      <ul class="d9-queue-legend" aria-label="Queue depth legend">
        ${repeat(
          QUEUE_LEGEND,
          (e) => e.label,
          (e) => html`
            <li class="d9-queue-legend-row">
              <span
                class="d9-queue-legend-swatch shape-${e.shape}"
                style="background:${e.swatch};color:${e.swatch}"
              ></span>
              <span class="d9-queue-legend-text">${e.label}</span>
            </li>
          `,
        )}
      </ul>
    `;
  }

  // ── M7 drag-select (dispatch through the selection slice) ──────────────

  function xToNs(
    canvas: HTMLCanvasElement,
    clientX: number,
    viewStart: number,
    viewEnd: number,
  ): number {
    const rect = canvas.getBoundingClientRect();
    const drawW = rect.width || 1;
    const mx = Math.min(Math.max(0, clientX - rect.left), drawW);
    return viewStart + (mx / drawW) * (viewEnd - viewStart);
  }

  function onCanvasMouseDown(ev: MouseEvent): void {
    const s = state();
    // M7 CONDITIONAL: only meaningful when the trace carries the active-task
    // timeline (legacy gate: `!trace || !activeTaskSamples.length`).
    if (s.trace.trace === null || !queueData().hasTaskTracking) return;
    if (s.viewport.viewEnd <= s.viewport.viewStart) return;
    const canvas = ev.currentTarget as HTMLCanvasElement;
    const startNs = xToNs(canvas, ev.clientX, s.viewport.viewStart, s.viewport.viewEnd);
    drag = { startX: ev.clientX, startNs, endNs: startNs, moved: false };
    document.body.style.cursor = "crosshair";
    // Stop the (future) lane-pan gesture from also starting on this canvas
    // (legacy called e.stopPropagation() on the queue-chart mousedown).
    ev.stopPropagation();
    window.addEventListener("mousemove", onWindowMouseMove);
    window.addEventListener("mouseup", onWindowMouseUp);
  }

  function onWindowMouseMove(ev: MouseEvent): void {
    if (drag === null || lastPaint === null) return;
    if (Math.abs(ev.clientX - drag.startX) > 3) drag.moved = true;
    drag.endNs = xToNs(lastPaint.canvas, ev.clientX, lastPaint.viewStart, lastPaint.viewEnd);
    // Redraw the chart from the cached model, then overlay the selection band.
    if (sizer === null) return;
    const ctx = sizer.ensure(lastPaint.drawW, lastPaint.canvasH, lastPaint.dpr);
    drawQueueCanvas(ctx, lastPaint.model, lastPaint.window, lastPaint.drawW, lastPaint.canvasH);
    if (drag.moved) {
      const selStart = Math.min(drag.startNs, drag.endNs);
      const selEnd = Math.max(drag.startNs, drag.endNs);
      overlayBandRange(ctx, lastPaint.drawW, lastPaint.canvasH, lastPaint.viewStart, lastPaint.viewEnd, selStart, selEnd);
    }
  }

  function onWindowMouseUp(): void {
    if (drag === null) return;
    const moved = drag.moved;
    const selStart = Math.min(drag.startNs, drag.endNs);
    const selEnd = Math.max(drag.startNs, drag.endNs);
    drag = null;
    document.body.style.cursor = "";
    window.removeEventListener("mousemove", onWindowMouseMove);
    window.removeEventListener("mouseup", onWindowMouseUp);
    if (moved) {
      // M7 dispatch: the range T31's inspector renders the spawned-task list
      // from. A plain click (no move) leaves the current range untouched
      // (legacy showSpawnedTasks only ran on qcDragMoved).
      commitRange({ startNs: selStart, endNs: selEnd });
    } else if (sizer !== null && lastPaint !== null) {
      // Repaint clean (drop any band drawn before the 3px threshold).
      const ctx = sizer.ensure(lastPaint.drawW, lastPaint.canvasH, lastPaint.dpr);
      drawQueueCanvas(ctx, lastPaint.model, lastPaint.window, lastPaint.drawW, lastPaint.canvasH);
    }
  }

  /** Overlay the active drag band (used when a store tick repaints mid-drag). */
  function overlayBand(
    ctx: CanvasRenderingContext2D,
    drawW: number,
    canvasH: number,
    viewStart: number,
    viewEnd: number,
  ): void {
    if (drag === null) return;
    const selStart = Math.min(drag.startNs, drag.endNs);
    const selEnd = Math.max(drag.startNs, drag.endNs);
    overlayBandRange(ctx, drawW, canvasH, viewStart, viewEnd, selStart, selEnd);
  }

  function commitRange(range: TimeRange | null): void {
    const cur = state().selection.spawnedTasksRange;
    if (range === null) {
      if (cur !== null) store.update("selection", { spawnedTasksRange: null });
      return;
    }
    if (cur !== null && cur.startNs === range.startNs && cur.endNs === range.endNs) return;
    store.update("selection", { spawnedTasksRange: range });
  }

  function data(): QueueData {
    return queueData();
  }

  function spawnedTasks(range: TimeRange): SpawnedTasksResult | null {
    return computeSpawnedTasks(queueData(), range);
  }

  function dispose(): void {
    if (drag !== null) {
      window.removeEventListener("mousemove", onWindowMouseMove);
      window.removeEventListener("mouseup", onWindowMouseUp);
      document.body.style.cursor = "";
      drag = null;
    }
    modelCache = null;
    lastPaint = null;
    sizer = null;
    sizerCanvas = null;
  }

  return { rowTemplate, paint, data, spawnedTasks, commitRange, dispose };
}

// ── Draw-area x mapping + selection band (module-local; A13 alignment) ─────

function nsToDrawX(ns: number, viewStart: number, viewEnd: number, drawW: number): number {
  const span = viewEnd - viewStart || 1;
  const x = ((ns - viewStart) / span) * drawW;
  return x < 0 ? 0 : x > drawW ? drawW : x;
}

/** Draw the green M7 selection band for [selStart, selEnd] over the canvas. */
function overlayBandRange(
  ctx: CanvasRenderingContext2D,
  drawW: number,
  canvasH: number,
  viewStart: number,
  viewEnd: number,
  selStart: number,
  selEnd: number,
): void {
  const x1 = nsToDrawX(selStart, viewStart, viewEnd, drawW);
  const x2 = nsToDrawX(selEnd, viewStart, viewEnd, drawW);
  ctx.fillStyle = SEL_FILL;
  ctx.fillRect(x1, 0, x2 - x1, canvasH);
  ctx.strokeStyle = SEL_STROKE;
  ctx.lineWidth = 1;
  ctx.strokeRect(x1, 0, x2 - x1, canvasH);
}

// ── Canvas draw (extracted so the render input is unit-testable) ───────────

/**
 * Draw the queue chart into `ctx` (already DPR-scaled + sized to drawW x
 * canvasH). Ports renderQueueChart's ops (viewer.html:4716-4892) in the legacy
 * order - background, global filled step-area (M1), max-local step line (M2),
 * active-task step line on its own scale (M3), y-axis labels (M4) - with the S6
 * scale change: every series maps through queueScaleY, which reserves a visible
 * zero baseline so a 0-valued global renders a flat line, not nothing. Then the
 * T17 window markers on top. Draw-area-relative coordinates (the canvas already
 * omits LABEL_W; the DOM label gutter supplies the shared offset - A13).
 */
export function drawQueueCanvas(
  ctx: CanvasRenderingContext2D,
  model: QueueRenderModel,
  window: QueueWindow,
  drawW: number,
  canvasH: number,
): void {
  ctx.clearRect(0, 0, drawW, canvasH);
  ctx.fillStyle = CANVAS_BG;
  ctx.fillRect(0, 0, drawW, canvasH);
  if (drawW <= 0 || canvasH <= 0) return;

  const chartTop = CHART_TOP;
  const chartH = canvasH - CHART_TOP - CHART_BOTTOM;
  if (chartH <= 0) return;

  if (!model.hasData) {
    ctx.fillStyle = EMPTY_TEXT;
    ctx.font = "11px sans-serif";
    ctx.textAlign = "left";
    ctx.fillText("No queue samples in view", 8, Math.min(chartTop + 14, canvasH / 2));
    return;
  }

  const n = model.numBuckets;
  const maxQ = model.maxQ;
  const baselineY = queueBaselineY(chartTop, chartH);

  // ── M1: global queue as a filled step area with a visible zero baseline ──
  // The fill polygon's bottom edge is the baseline (queueScaleY(0)), NOT the
  // true canvas bottom, so a 0-valued global is a thin sliver + a stroke ON the
  // baseline - the S6 visible flat line, not a stroke fused to the axis.
  ctx.beginPath();
  ctx.moveTo(0, baselineY);
  for (let i = 0; i < n; i++) {
    ctx.lineTo(i, queueScaleY(model.global[i]!, maxQ, chartTop, chartH));
  }
  ctx.lineTo(n - 1, baselineY);
  ctx.closePath();
  ctx.fillStyle = GLOBAL_FILL;
  ctx.fill();
  ctx.strokeStyle = GLOBAL_STROKE;
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let i = 0; i < n; i++) {
    const y = queueScaleY(model.global[i]!, maxQ, chartTop, chartH);
    if (i === 0) ctx.moveTo(i, y);
    else ctx.lineTo(i, y);
  }
  ctx.stroke();

  // ── M2: max local queue as a step line ─────────────────────────────────
  ctx.beginPath();
  for (let i = 0; i < n; i++) {
    const y = queueScaleY(model.local[i]!, maxQ, chartTop, chartH);
    if (i === 0) ctx.moveTo(i, y);
    else ctx.lineTo(i, y);
  }
  ctx.strokeStyle = LOCAL_STROKE;
  ctx.lineWidth = 1;
  ctx.stroke();

  // ── M4: y-axis labels (max at top, 0 at the visible baseline) ───────────
  ctx.fillStyle = AXIS_LABEL;
  ctx.font = "10px monospace";
  ctx.textAlign = "left";
  ctx.fillText(String(maxQ), 2, chartTop + 8);
  ctx.fillText("0", 2, baselineY - 1);

  // ── M3: active-task count as a step line on its own magnitude scale ─────
  const at = model.activeTask;
  if (at !== null) {
    ctx.fillStyle = ACTIVE_LABEL;
    ctx.font = "10px monospace";
    ctx.textAlign = "right";
    ctx.fillText(`tasks:${at.maxTasks}`, Math.max(0, drawW - 4), chartTop + 8);

    ctx.beginPath();
    let y = queueScaleY(at.startCount, at.maxTasks, chartTop, chartH);
    ctx.moveTo(0, y);
    for (const p of at.points) {
      ctx.lineTo(p.x, y);
      y = queueScaleY(p.count, at.maxTasks, chartTop, chartH);
      ctx.lineTo(p.x, y);
    }
    ctx.lineTo(drawW, y);
    ctx.strokeStyle = ACTIVE_STROKE;
    ctx.lineWidth = 1.5;
    ctx.stroke();
  }

  // ── T17: surface a truncated / oversized window (never as complete) ─────
  drawWindowMarkers(ctx, window, drawW, canvasH);
}

/**
 * Surface the resident-window state (T17 obligation): a translucent hatch band
 * at each truncated edge (so missing samples read as "not loaded", not "queue
 * empty"), and a "partial window" badge when a needed segment is oversized.
 * No-ops for a complete window. Mirrors cpu.ts / task-detail-track.ts.
 */
function drawWindowMarkers(
  ctx: CanvasRenderingContext2D,
  window: QueueWindow,
  drawW: number,
  canvasH: number,
): void {
  if (window === COMPLETE_QUEUE_WINDOW) return;
  const BAND = 6;
  const t = window.truncatedAt;
  if (t === "start" || t === "both") {
    ctx.fillStyle = TRUNC_BAND;
    ctx.fillRect(0, 0, BAND, canvasH);
  }
  if (t === "end" || t === "both") {
    ctx.fillStyle = TRUNC_BAND;
    ctx.fillRect(Math.max(0, drawW - BAND), 0, BAND, canvasH);
  }
  if (window.oversized) {
    ctx.fillStyle = TRUNC_LABEL;
    ctx.font = "10px monospace";
    ctx.textAlign = "left";
    ctx.fillText("partial window", 3, canvasH - 3);
  }
}
