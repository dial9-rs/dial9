// The Queue depth track component: the "queue" row's template (label + legend +
// canvas) and its canvas paint. Everything reactive flows through the store;
// the heavy series derivation is a store.derived cache over the `trace` slice
// and the per-frame render model is memoized on its primitive inputs.
//
// The global + max-local + active-task series render against ONE explicit zero
// baseline (queueScaleY) so a 0-valued global queue shows a VISIBLE flat line
// instead of a stroke fused to the axis; the numbers are unchanged, only the
// y-mapping and the labeling/legend.
//
// Hover info rides the overlay's at-cursor store contract (transient.atCursor),
// not a corner div, so hovering this track already populates the inspector
// readout - no separate hover dispatch here. A drag-select dispatches the range
// to selection.spawnedTasksRange; the inspector renders the spawned-tasks list
// (computeSpawnedTasks is the shared derivation). A segment-windowed view can
// be missing samples beyond the resident window; drawWindowMarkers surfaces a
// truncated/oversized window rather than painting a clipped chart as complete.

import { drawWindowMarkers } from "./resident-window.js";
import { html, type TemplateResult } from "lit-html";
import { repeat } from "lit-html/directives/repeat.js";
import { clampX, createCanvasSizer, nsToDrawX } from "../../lib/canvas/index.js";
import type { CanvasSizer } from "../../lib/canvas/index.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import type { TimeRange } from "../../types/trace.js";
import type { TrackSpec } from "../../lib/canvas/track-layout.js";
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

// Queue-panel colours.
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

// Vertical chart margins inside the canvas (below the legend strip). Headroom
// at top for the maxQ label, a little at the bottom for the "0" label; the
// explicit zero baseline (queueScaleY) sits just above CHART_BOTTOM.
const CHART_TOP = 12;
const CHART_BOTTOM = 6;

/** The handle tracks.ts + the shell drive. */
export interface QueueTrackController {
  /** The full `.d9-track` row template for the queue track. */
  rowTemplate(track: TrackSpec): TemplateResult;
  /** Size + draw the queue canvas. Called from sizeTracks inside the shell's
   *  frame tick. */
  paint(
    canvas: HTMLCanvasElement,
    drawW: number,
    trackHeight: number,
    dpr: number,
    viewStart: number,
    viewEnd: number,
  ): void;
  queueSeries(): QueueData;
  /**
   * The spawned-tasks derivation for a range: tasks first polled in range,
   * grouped by spawn location. Exposed so the inspector reuses the logic and
   * so the dispatch is testable without a canvas.
   */
  spawnedTasks(range: TimeRange): SpawnedTasksResult | null;
  /**
   * Write the drag-selected range to `selection.spawnedTasksRange` (null clears
   * it). DOM-free so the dispatch is testable. Only writes on an actual change
   * (no store thrash).
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
  // (a range dispatch) reuses the cached buckets instead of rebuilding them.
  let modelCache: { data: QueueData; key: string; model: QueueRenderModel } | null = null;
  let sizer: CanvasSizer<CanvasRenderingContext2D> | null = null;
  let sizerCanvas: HTMLCanvasElement | null = null;

  // The geometry of the last paint, so the drag handler can repaint the chart +
  // selection band without going through the store.
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

  // Drag-select state (self-contained). The gesture is queue-track-local; it
  // does NOT set transient.drag (that slice is the lane pan/region/zoom).
  let drag: { startX: number; startNs: number; endNs: number; moved: boolean } | null =
    null;

  function state(): StoreState {
    return store.getState();
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

  // ── Canvas paint ────────────────────────────────────────────────────────

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

  // ── Templates (label + legend) ──────────────────────────────────────────

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

  /** The in-track legend, swatch encodings matching the draw. */
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

  // ── Drag-select (dispatch through the selection slice) ──────────────────

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
    // Only meaningful when the trace carries the active-task timeline.
    if (s.trace.trace === null || !queueData().hasTaskTracking) return;
    if (s.viewport.viewEnd <= s.viewport.viewStart) return;
    const canvas = ev.currentTarget as HTMLCanvasElement;
    const startNs = xToNs(canvas, ev.clientX, s.viewport.viewStart, s.viewport.viewEnd);
    drag = { startX: ev.clientX, startNs, endNs: startNs, moved: false };
    document.body.style.cursor = "crosshair";
    // Stop the lane-pan gesture from also starting on this canvas.
    ev.stopPropagation();
    window.addEventListener("mousemove", onWindowMouseMove);
    window.addEventListener("mouseup", onWindowMouseUp);
  }

  /**
   * Repaint the drag band, at most once per frame.
   *
   * mousemove fires faster than the display refreshes (well past 60/s on a
   * high-polling-rate mouse), and this path draws the whole canvas. Unlike
   * every other repaint in the viewer it does NOT go through the store, so
   * nothing was coalescing it. Gated here the same way the lanes' inner scroll
   * is (components/canvas/lanes/index.ts).
   */
  let dragFrame = 0;
  function scheduleDragPaint(): void {
    if (dragFrame !== 0) return;
    dragFrame = requestAnimationFrame(() => {
      dragFrame = 0;
      if (drag === null || lastPaint === null || sizer === null) return;
      const ctx = sizer.ensure(lastPaint.drawW, lastPaint.canvasH, lastPaint.dpr);
      drawQueueCanvas(ctx, lastPaint.model, lastPaint.window, lastPaint.drawW, lastPaint.canvasH);
      if (drag.moved) {
        const selStart = Math.min(drag.startNs, drag.endNs);
        const selEnd = Math.max(drag.startNs, drag.endNs);
        overlayBandRange(ctx, lastPaint.drawW, lastPaint.canvasH, lastPaint.viewStart, lastPaint.viewEnd, selStart, selEnd);
      }
    });
  }

  function cancelDragPaint(): void {
    if (dragFrame === 0) return;
    cancelAnimationFrame(dragFrame);
    dragFrame = 0;
  }

  function onWindowMouseMove(ev: MouseEvent): void {
    if (drag === null || lastPaint === null) return;
    if (Math.abs(ev.clientX - drag.startX) > 3) drag.moved = true;
    drag.endNs = xToNs(lastPaint.canvas, ev.clientX, lastPaint.viewStart, lastPaint.viewEnd);
    scheduleDragPaint();
  }

  function onWindowMouseUp(): void {
    if (drag === null) return;
    const moved = drag.moved;
    const selStart = Math.min(drag.startNs, drag.endNs);
    const selEnd = Math.max(drag.startNs, drag.endNs);
    drag = null;
    document.body.style.cursor = "";
    // Drop any frame still queued from the last mousemove: it would repaint a
    // band for a drag that has ended.
    cancelDragPaint();
    window.removeEventListener("mousemove", onWindowMouseMove);
    window.removeEventListener("mouseup", onWindowMouseUp);
    if (moved) {
      // Dispatch the range the inspector renders the spawned-task list from. A
      // plain click (no move) leaves the current range untouched.
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

  function queueSeries(): QueueData {
    return queueData();
  }

  function spawnedTasks(range: TimeRange): SpawnedTasksResult | null {
    return computeSpawnedTasks(queueData(), range);
  }

  function dispose(): void {
    cancelDragPaint();
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

  return { rowTemplate, paint, queueSeries, spawnedTasks, commitRange, dispose };
}

// ── Draw-area x mapping + selection band (module-local) ────────────────────


/** Draw the green selection band for [selStart, selEnd] over the canvas. */
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
 * canvasH) in order - background, global filled step-area, max-local step line,
 * active-task step line on its own scale, y-axis labels - every series mapped
 * through queueScaleY, which reserves a visible zero baseline so a 0-valued
 * global renders a flat line, not nothing. Then the window markers on top.
 * Draw-area-relative coordinates (the canvas already omits LABEL_W; the DOM
 * label gutter supplies the shared offset).
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

  // ── Global queue as a filled step area with a visible zero baseline ──
  // The fill polygon's bottom edge is the baseline (queueScaleY(0)), NOT the
  // true canvas bottom, so a 0-valued global is a thin sliver + a stroke ON the
  // baseline - a visible flat line, not a stroke fused to the axis.
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

  // ── Max local queue as a step line ──────────────────────────────────────
  ctx.beginPath();
  for (let i = 0; i < n; i++) {
    const y = queueScaleY(model.local[i]!, maxQ, chartTop, chartH);
    if (i === 0) ctx.moveTo(i, y);
    else ctx.lineTo(i, y);
  }
  ctx.strokeStyle = LOCAL_STROKE;
  ctx.lineWidth = 1;
  ctx.stroke();

  // ── Y-axis labels (max at top, 0 at the visible baseline) ───────────────
  ctx.fillStyle = AXIS_LABEL;
  ctx.font = "10px monospace";
  ctx.textAlign = "left";
  ctx.fillText(String(maxQ), 2, chartTop + 8);
  ctx.fillText("0", 2, baselineY - 1);

  // ── Active-task count as a step line on its own magnitude scale ─────────
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

  // ── Surface a truncated / oversized window (never as complete) ─────
  drawWindowMarkers(ctx, window, drawW, canvasH);
}

/**
 * Surface the resident-window state: a translucent hatch band at each
 * truncated edge (so missing samples read as "not loaded", not "queue empty"),
 * and a "partial window" badge when a needed segment is oversized. No-ops for a
 * complete window. Mirrors cpu.ts.
 */
