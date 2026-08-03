// The task-detail track component. Hosts inside the unified track column:
// tracks.ts delegates the "task-detail" row's template (label + status readout +
// canvas) and its canvas paint here. The track is selection-only, so tracks.ts
// renders it only while a task is selected.
//
// Everything reactive flows through the store:
//   - The task collection is a store.derived cache keyed on ["trace",
//     "selection"]: selecting a DIFFERENT task invalidates it, a PAN does not. An
//     inner (source, taskId) guard means a hover-only selection write (the waker
//     highlight) does NOT re-walk every worker's polls - only a genuine task
//     change does.
//   - The per-frame render model is memoized on its primitive inputs, so a hover
//     redraw reuses the cached geometry instead of rebuilding it.
//   - Waker hover dispatches `selection.hoveredWakerTaskId`. The lanes consume
//     that store field for the orange-poll highlight - the store field IS the
//     contract; this track never reaches into the lanes.
//
// This file owns the timeline track (per-task polls/wakes over time) + the
// derivation. The textual detail (task id, spawn location, counts, the
// uninstrumented badge, the idle-flamegraph link) renders in the inspector Task
// tab from the SAME derivation, exposed via createTaskDetailDerivation.

import { drawWindowMarkers } from "./resident-window.js";
import { html, nothing, type TemplateResult } from "lit-html";
import { createCanvasSizer } from "../../lib/canvas/index.js";
import type { CanvasSizer } from "../../lib/canvas/index.js";
import { formatHumanDuration } from "../../lib/trace/index.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import type { TrackSpec } from "../../lib/canvas/track-layout.js";
import {
  BAND_H,
  BAND_TOP,
  COMPLETE_TASK_DETAIL_WINDOW,
  EMPTY_TASK_DETAIL_DATA,
  buildTaskDetailRenderModel,
  buildTaskPollSource,
  computeTaskDetailData,
  formatTaskDetailSummary,
  hitRegionAt,
  statusTextAt,
  wakeRegionAt,
  type TaskDetailData,
  type TaskDetailRenderModel,
  type TaskDetailWindow,
  type TaskPollSource,
} from "./task-detail-model.js";

// Task-detail canvas colours.
const CANVAS_BG = "#16213e";
const POLL_FILL = "#4fc3f7";
const WAKE_TRIANGLE = "#66bb6a";
const LIFESPAN_FILL = "rgba(129,199,132,0.08)";
const LIFESPAN_BORDER = "rgba(129,199,132,0.3)";
const LIFESPAN_EDGE = "#81c784";
const LEGEND_LABEL = "#aaa";
const CROSSHATCH = "rgba(140,120,255,0.35)";

/**
 * The store-cached task-detail derivation. Keyed on ["trace", "selection"] so a
 * task change invalidates it and a pan does not; an inner (source, taskId) memo
 * keeps a hover-only selection write (hoveredWakerTaskId) from re-walking the
 * collection. Exposed as a factory so the inspector Task tab reads the SAME
 * derived data without rebuilding the logic.
 */
export function createTaskDetailDerivation(store: ViewerStore): () => TaskDetailData {
  // buildWorkerSpans scans every event; cache it on the trace slice so it is
  // rebuilt only on load/reparse, never per selection/pan.
  const source = store.derived(["trace"], (s): TaskPollSource =>
    buildTaskPollSource(s.trace.trace),
  );
  let memo: { source: TaskPollSource; taskId: number | null; data: TaskDetailData } | null =
    null;
  return store.derived(["trace", "selection"], (s): TaskDetailData => {
    const src = source();
    const taskId = s.selection.selectedTaskId;
    // Inner guard: a selection write that did NOT change the task (e.g. a
    // waker hover) reuses the last collection instead of re-walking it.
    if (memo !== null && memo.source === src && memo.taskId === taskId) {
      return memo.data;
    }
    const data = computeTaskDetailData(src, s.trace.trace, taskId);
    memo = { source: src, taskId, data };
    return data;
  });
}

/**
 * The window descriptor from the store: any needed segment stuck "oversized"
 * makes the poll list unavoidably partial. The whole-trace shell has an empty
 * segments slice, so this resolves to complete; the renderer consumes the
 * descriptor regardless.
 */
export function deriveTaskDetailWindow(state: StoreState): TaskDetailWindow {
  for (const entry of state.segments.segments.values()) {
    if (entry.state === "oversized") return { truncatedAt: null, oversized: true };
  }
  return COMPLETE_TASK_DETAIL_WINDOW;
}

/** The handle tracks.ts + the shell drive. */
export interface TaskDetailTrackController {
  /** The full `.d9-track` row template for the task-detail track. */
  rowTemplate(track: TrackSpec): TemplateResult;
  /** Size + draw the task-detail canvas. Called from sizeTracks inside the
   *  shell's frame tick. */
  paint(
    canvas: HTMLCanvasElement,
    drawW: number,
    trackHeight: number,
    dpr: number,
    viewStart: number,
    viewEnd: number,
  ): void;
  /** Read the current (cached) derivation - the inspector Task tab source. */
  taskDetail(): TaskDetailData;
  /**
   * Dispatch `selection.hoveredWakerTaskId` for a pointer at draw-area (mx, my)
   * over `model` (null off any waker label). Dispatches only on a change (no
   * store thrash). The DOM hover handler delegates here; exposed so the dispatch
   * is testable without a canvas/DOM.
   */
  hoverWaker(model: TaskDetailRenderModel, mx: number, my: number): void;
  /** Dispatch a task-detail canvas click to its waker or task-dump target. */
  clickAt(model: TaskDetailRenderModel, mx: number, my: number): void;
  /** Clear the waker highlight (pointer left the canvas). */
  clearHover(): void;
  /** Tear down (no external resources today; symmetry with other tracks). */
  dispose(): void;
}

export function createTaskDetailTrack(store: ViewerStore): TaskDetailTrackController {
  const taskDetailData = createTaskDetailDerivation(store);

  // Per-frame render-model memo, keyed on the derived-data reference (task
  // identity) + the primitive viewport/width inputs, so a hover-only redraw
  // reuses the cached geometry (buildTaskDetailRenderModel is not re-run).
  let modelCache: { data: TaskDetailData; key: string; model: TaskDetailRenderModel } | null =
    null;
  // The model last painted, for canvas hit-testing (status, waker).
  let lastModel: TaskDetailRenderModel | null = null;
  let sizer: CanvasSizer<CanvasRenderingContext2D> | null = null;
  let sizerCanvas: HTMLCanvasElement | null = null;

  function state(): StoreState {
    return store.getState();
  }

  function renderModel(
    data: TaskDetailData,
    viewStart: number,
    viewEnd: number,
    drawW: number,
  ): TaskDetailRenderModel {
    const key = `${viewStart}|${viewEnd}|${drawW}`;
    if (modelCache !== null && modelCache.data === data && modelCache.key === key) {
      return modelCache.model;
    }
    const model = buildTaskDetailRenderModel({ data, viewStart, viewEnd, drawW });
    modelCache = { data, key, model };
    return model;
  }

  // ── Canvas paint ───────────────────────────────────────────────────────

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
    const ctx = sizer.ensure(drawW, trackHeight, dpr);
    const s = state();
    const data = taskDetailData();
    const model = renderModel(data, viewStart, viewEnd, drawW);
    lastModel = model;
    drawTaskDetailCanvas(
      ctx,
      model,
      s.selection.hoveredWakerTaskId,
      drawW,
      trackHeight,
      deriveTaskDetailWindow(s),
    );
  }

  // ── Template (identity + status) ───────────────────────────────────────

  function rowTemplate(track: TrackSpec): TemplateResult {
    const data = taskDetailData();
    const identity =
      data.taskId !== null ? formatTaskDetailSummary(data) : track.label;
    return html`
      <div
        class="d9-track d9-track--task-detail"
        data-track-id=${track.id}
        style="height:${track.height}px"
      >
        <div class="d9-track-label" id="d9-track-label-task-detail">
          <span class="d9-track-name">${track.label}</span>
          ${data.taskId !== null
            ? html`<span class="d9-task-detail-id" title=${identity}
                >Task 0x${data.taskId.toString(16)}</span
              >`
            : nothing}
        </div>
        <div class="d9-track-canvas-wrap d9-task-detail-wrap">
          <span
            class="d9-task-detail-status"
            role="status"
            aria-live="polite"
          ></span>
          <canvas
            class="d9-track-canvas d9-task-detail-canvas"
            data-track-canvas=${track.id}
            aria-labelledby="d9-track-label-task-detail"
            role="img"
            @mousemove=${onCanvasHover}
            @mouseleave=${onCanvasLeave}
            @click=${onCanvasClick}
          ></canvas>
        </div>
      </div>
    `;
  }

  // ── Canvas interaction (status, waker hover/click) ─────────────────────

  function pointerAt(canvas: HTMLCanvasElement, ev: MouseEvent): { mx: number; my: number } {
    const rect = canvas.getBoundingClientRect();
    return { mx: ev.clientX - rect.left, my: ev.clientY - rect.top };
  }

  function statusEl(canvas: HTMLCanvasElement): HTMLElement | null {
    return (
      canvas.parentElement?.querySelector<HTMLElement>(".d9-task-detail-status") ??
      null
    );
  }

  // ── Store-dispatch seams (model-driven, DOM-free - testable) ───────────

  function hoverWaker(model: TaskDetailRenderModel, mx: number, my: number): void {
    // Waker-label hover -> dispatch selection.hoveredWakerTaskId (the store
    // field the lanes consume). Only dispatch on an actual change, so a hover
    // inside the same region does not thrash the store.
    const waker = wakeRegionAt(model, mx, my);
    const found = waker !== null ? waker.wakerTaskId : null;
    if (found !== state().selection.hoveredWakerTaskId) {
      store.update("selection", { hoveredWakerTaskId: found });
    }
  }

  function clickWaker(model: TaskDetailRenderModel, mx: number, my: number): void {
    // Click a waker label -> select the waker task (re-scopes the whole detail
    // to that task). Clear the hover highlight so the newly selected task starts
    // clean.
    const waker = wakeRegionAt(model, mx, my);
    if (waker !== null) {
      store.update("selection", {
        selectedTaskId: waker.wakerTaskId,
        hoveredWakerTaskId: null,
        taskDump: null,
      });
    }
  }

  function clickTaskDump(
    model: TaskDetailRenderModel,
    mx: number,
    my: number,
  ): void {
    const hit = hitRegionAt(model, mx, my);
    const dumps = hit?.dumps ?? null;
    const selectedTaskId = state().selection.selectedTaskId;
    if (
      model.taskId === null ||
      selectedTaskId !== model.taskId ||
      dumps === null ||
      dumps.length === 0
    ) {
      return;
    }
    store.update("selection", {
      taskDump: {
        taskId: model.taskId,
        timestamps: dumps.map((dump) => dump.timestamp),
      },
      pinnedEvent: null,
      pollDetail: null,
      sidebarRange: null,
      spawnedTasksRange: null,
    });
  }

  function clickAt(model: TaskDetailRenderModel, mx: number, my: number): void {
    if (wakeRegionAt(model, mx, my) !== null) {
      clickWaker(model, mx, my);
    } else {
      clickTaskDump(model, mx, my);
    }
  }

  function clearHover(): void {
    if (state().selection.hoveredWakerTaskId !== null) {
      store.update("selection", { hoveredWakerTaskId: null });
    }
  }

  // ── DOM event handlers (thin wrappers over the seams above) ────────────

  function onCanvasHover(ev: MouseEvent): void {
    const canvas = ev.currentTarget as HTMLCanvasElement;
    const model = lastModel;
    if (model === null) return;
    const { mx, my } = pointerAt(canvas, ev);

    hoverWaker(model, mx, my);

    // Status readout (imperative, hover-only - no store, no layout read).
    const status = statusEl(canvas);
    if (status !== null) status.textContent = statusTextAt(model, mx, my);

    // Cursor: pointer over a waker label or a dumped idle region.
    const waker = wakeRegionAt(model, mx, my);
    const hit = hitRegionAt(model, mx, my);
    const clickable =
      waker !== null || (hit !== null && hit.dumps !== null && hit.dumps.length > 0);
    canvas.style.cursor = clickable ? "pointer" : "";
  }

  function onCanvasLeave(ev: MouseEvent): void {
    const canvas = ev.currentTarget as HTMLCanvasElement;
    const status = statusEl(canvas);
    if (status !== null) status.textContent = "";
    clearHover();
  }

  function onCanvasClick(ev: MouseEvent): void {
    const canvas = ev.currentTarget as HTMLCanvasElement;
    const model = lastModel;
    if (model === null) return;
    const { mx, my } = pointerAt(canvas, ev);
    clickAt(model, mx, my);
  }

  function taskDetail(): TaskDetailData {
    return taskDetailData();
  }

  function dispose(): void {
    modelCache = null;
    lastModel = null;
    sizer = null;
    sizerCanvas = null;
  }

  return {
    rowTemplate,
    paint,
    taskDetail,
    hoverWaker,
    clickAt,
    clearHover,
    dispose,
  };
}

// ── Canvas draw (extracted so the render input is unit-testable) ───────────

/**
 * Draw the task-detail timeline into `ctx` (already DPR-scaled + sized to
 * drawW x height), in order: background, lifespan bar, scheduling-delay bands,
 * poll bars / coverage histogram, idle gaps, legend, then the window markers
 * on top. Draw-area-relative coordinates (the model's x's already omit
 * LABEL_W).
 */
export function drawTaskDetailCanvas(
  ctx: CanvasRenderingContext2D,
  model: TaskDetailRenderModel,
  hoveredWakerTaskId: number | null,
  drawW: number,
  height: number,
  window: TaskDetailWindow,
): void {
  ctx.clearRect(0, 0, drawW, height);
  ctx.fillStyle = CANVAS_BG;
  ctx.fillRect(0, 0, drawW, height);
  if (drawW <= 0 || height <= 0) return;

  ctx.font = "9px monospace";

  // ── Task lifespan bar ────────────────────────────────────────────────
  if (model.lifespan !== null) {
    const l = model.lifespan;
    ctx.fillStyle = LIFESPAN_FILL;
    ctx.fillRect(l.x1, BAND_TOP - 2, l.x2 - l.x1, BAND_H + 4);
    ctx.strokeStyle = LIFESPAN_BORDER;
    ctx.lineWidth = 1;
    ctx.setLineDash([]);
    ctx.beginPath();
    ctx.moveTo(l.x1, BAND_TOP - 2);
    ctx.lineTo(l.x2, BAND_TOP - 2);
    ctx.moveTo(l.x1, BAND_TOP + BAND_H + 2);
    ctx.lineTo(l.x2, BAND_TOP + BAND_H + 2);
    ctx.stroke();
    if (l.showSpawn) {
      ctx.strokeStyle = LIFESPAN_EDGE;
      ctx.beginPath();
      ctx.moveTo(l.x1, BAND_TOP - 6);
      ctx.lineTo(l.x1, BAND_TOP + BAND_H + 6);
      ctx.stroke();
      ctx.fillStyle = LIFESPAN_EDGE;
      ctx.font = "8px monospace";
      ctx.textAlign = "left";
      ctx.fillText("spawn▸", l.x1 + 2, BAND_TOP - 8);
    }
    if (l.showDone) {
      ctx.strokeStyle = LIFESPAN_EDGE;
      ctx.beginPath();
      ctx.moveTo(l.x2, BAND_TOP - 6);
      ctx.lineTo(l.x2, BAND_TOP + BAND_H + 6);
      ctx.stroke();
      ctx.fillStyle = LIFESPAN_EDGE;
      ctx.font = "8px monospace";
      ctx.textAlign = "right";
      ctx.fillText("◂done", l.x2 - 2, BAND_TOP - 8);
    }
    ctx.font = "9px monospace";
  }

  // ── Scheduling-delay bands + wake labels ────────────────────────────
  for (const band of model.schedulingBands) {
    const w = band.x2 - band.x1;
    ctx.fillStyle =
      band.severity === "high"
        ? "rgba(255,50,50,0.3)"
        : band.severity === "mid"
          ? "rgba(255,150,50,0.3)"
          : "rgba(100,200,100,0.15)";
    ctx.fillRect(band.x1, BAND_TOP, w, BAND_H);

    ctx.strokeStyle =
      band.severity === "high" ? "#ff4444" : band.severity === "mid" ? "#ff8a65" : "#555";
    ctx.lineWidth = 1;
    ctx.setLineDash([2, 2]);
    ctx.strokeRect(band.x1, BAND_TOP, w, BAND_H);
    ctx.setLineDash([]);

    if (band.showDelayLabel) {
      ctx.fillStyle =
        band.severity === "high" ? "#ff4444" : band.severity === "mid" ? "#ff8a65" : "#888";
      ctx.textAlign = "center";
      ctx.fillText(formatHumanDuration(band.delayNs), band.x1 + w / 2, BAND_TOP - 6);
    }

    if (band.kind !== "wake") continue;

    // Wake marker (triangle) at the band's left edge.
    const wx = band.x1;
    ctx.fillStyle = WAKE_TRIANGLE;
    ctx.beginPath();
    ctx.moveTo(wx, BAND_TOP + BAND_H + 2);
    ctx.lineTo(wx - 4, BAND_TOP + BAND_H + 9);
    ctx.lineTo(wx + 4, BAND_TOP + BAND_H + 9);
    ctx.closePath();
    ctx.fill();

    if (band.showWakerLabel) {
      const isHovered = hoveredWakerTaskId === band.wakerTaskId;
      ctx.fillStyle = isHovered ? "#fff" : "#66bb6a";
      ctx.font = isHovered ? "bold 8px monospace" : "8px monospace";
      ctx.textAlign = "left";
      ctx.fillText("⬆ " + band.wakerLabel, wx, BAND_TOP + BAND_H + 20);
      ctx.font = "9px monospace";
    }
  }

  // ── Polling sections: coverage histogram or per-poll bars ────────────
  ctx.textAlign = "center";
  if (model.coverage !== null) {
    const cov = model.coverage;
    ctx.fillStyle = POLL_FILL;
    for (let px = 0; px < cov.length; px++) {
      const c = cov[px]!;
      if (c <= 0) continue;
      ctx.globalAlpha = Math.min(1, 0.15 + c * 0.85);
      ctx.fillRect(px, BAND_TOP, 1, BAND_H);
    }
    ctx.globalAlpha = 1;
  } else {
    for (const bar of model.pollBars) {
      const w = bar.x2 - bar.x1;
      ctx.fillStyle = POLL_FILL;
      ctx.fillRect(bar.x1, BAND_TOP, w, BAND_H);
      if (bar.showLabel) {
        ctx.fillStyle = "#000";
        ctx.fillText(
          formatHumanDuration(bar.durationNs),
          bar.x1 + w / 2,
          BAND_TOP + BAND_H / 2 + 3,
        );
      }
    }
  }

  // ── Idle gaps ────────────────────────────────────────────────────────
  for (const idle of model.idleBands) {
    const w = idle.x2 - idle.x1;
    ctx.fillStyle = idle.hasDump ? "#2a2a5a" : "#2a2a4a";
    ctx.fillRect(idle.x1, BAND_TOP, w, BAND_H);
    if (idle.hasDump) drawCrossHatch(ctx, idle.x1, BAND_TOP, w, BAND_H);
    ctx.strokeStyle = idle.hasDump ? "#7c6cff" : "#444";
    ctx.lineWidth = 1;
    ctx.setLineDash([3, 3]);
    ctx.strokeRect(idle.x1, BAND_TOP, w, BAND_H);
    ctx.setLineDash([]);
    if (idle.showLabel) {
      ctx.fillStyle = idle.durationNs > 1e6 ? "#ff8a65" : "#888";
      ctx.textAlign = "center";
      ctx.fillText(formatHumanDuration(idle.durationNs), idle.x1 + w / 2, BAND_TOP - 6);
    }
  }

  // ── Legend ───────────────────────────────────────────────────────────
  ctx.fillStyle = LEGEND_LABEL;
  ctx.font = "9px monospace";
  ctx.textAlign = "left";
  ctx.fillText("Task", 6, BAND_TOP + BAND_H / 2 + 4);
  ctx.fillStyle = WAKE_TRIANGLE;
  ctx.fillText("▲ = wake", 6, BAND_TOP + BAND_H + 18);

  // ── Surface a truncated / oversized window (never as complete) ───────
  drawWindowMarkers(ctx, window, drawW, height);
}

/**
 * Diagonal cross-hatch inside (x, y, w, h), clipped - flags idle periods with a
 * captured task dump.
 */
function drawCrossHatch(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
): void {
  const step = 8;
  ctx.save();
  ctx.beginPath();
  ctx.rect(x, y, w, h);
  ctx.clip();
  ctx.strokeStyle = CROSSHATCH;
  ctx.lineWidth = 1;
  ctx.setLineDash([]);
  for (let hx = x - h; hx < x + w; hx += step) {
    ctx.beginPath();
    ctx.moveTo(hx, y + h);
    ctx.lineTo(hx + h, y);
    ctx.stroke();
  }
  ctx.restore();
}

/**
 * Surface the resident-window state: a translucent hatch band at each truncated
 * edge (so missing polls read as "not loaded", not "no activity"), and a
 * "partial window" badge when a needed segment is oversized. No-ops for a
 * complete window. Mirrors the CPU track's drawWindowMarkers.
 */
