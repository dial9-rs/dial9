// src/pages/viewer/events-track.ts - the custom-events track component (T27;
// features/02 section K). Hosts inside T21's unified track column: tracks.ts
// delegates the "events" row's TEMPLATE (legend chips + canvas) and its canvas
// PAINT here. Everything reactive flows through the store (T07): the name-chip
// filter writes uiPrefs.selectedEventNames, a click PINS the event
// (selection.pinnedEvent), a hover writes the guide-line timestamp
// (transient.hoverEventTs); the canvas redraws inside the shell's frame tick
// (never a direct out-of-scheduler render, N18/F2).
//
// The heavy derivations are cached the way perf finding F5 prescribes:
//   - The trace-invariant event data (computeEventTrackData) is a
//     store.derived(["trace"], ...) cache - recomputed only when the trace
//     slice is replaced, NOT per render.
//   - The per-frame render model (buildEventRenderModel) is memoized on its
//     primitive inputs, so a selection-only change (S4 dimming) redraws with
//     the SAME cached geometry instead of recomputing it.
//   - The per-event task resolver (K7) is memoized in a WeakMap (legacy
//     `_taskForEventCache`), rebuilt only when the worker lanes change.
// The visible-event window is BINARY-SEARCHED at both edges (events-model
// filterVisibleEvents) - the legacy O(all generic events) per-frame scan (03
// F5) is gone, per 03 F6's binary-search-helpers rule. This is the perf fix
// T27 owns.
//
// SEAMS (scope fence, T27 ticket):
//   - The hover guide line (I4) and pinned-event marker (I5) are drawn by the
//     T24 overlay (components/overlay/crosshair.ts) reading transient.
//     hoverEventTs + selection.pinnedEvent. This track DISPATCHES those slices;
//     it does NOT draw the overlay marks.
//   - The event-KV sidebar is T31's surface. This track dispatches selection.
//     pinnedEvent (the PinnedCustomEvent contract, types/state.d.ts); T31
//     renders it. Until T31 lands the pin still drives the overlay marker + the
//     track dimming, which is observable today.
//
// T17 carried obligation (audit notes 6+7): a segment stuck "oversized" makes
// this view unavoidably partial; the canvas surfaces a "partial window" badge
// rather than presenting the loaded ticks as the whole event stream. (Point
// markers have no continuous edge to hatch, unlike the CPU series, so only the
// oversized state is surfaced.)

import { html, render, nothing, type TemplateResult } from "lit-html";
import { repeat } from "lit-html/directives/repeat.js";
import { classMap } from "lit-html/directives/class-map.js";
import { createCanvasSizer, makeColorAssigner } from "../../lib/canvas/index.js";
import type { CanvasSizer } from "../../lib/canvas/index.js";
import { buildWorkerSpans, formatFieldValue } from "../../lib/trace/index.js";
import type {
  CustomTraceEvent,
  ParsedTrace,
  WorkerLane,
} from "../../lib/trace/index.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import { deriveAxisInputs, fmtAxisTick } from "./axis.js";
import type { TrackSpec } from "./track-layout.js";
import {
  buildEventRenderModel,
  buildPinnedEvent,
  computeEventTrackData,
  eventHighlightTask,
  isSameCluster,
  resolveTaskForEvent,
  EVENT_DIM_FACTOR,
  type EventDrawBucket,
  type EventRenderModel,
  type EventTrackData,
} from "./events-model.js";

/** Height reserved for the legend strip above the event canvas (features/02
 *  K5/K6). */
export const LEGEND_H = 26;

// Legacy custom-events panel colors (viewer.html), kept identical.
const CANVAS_BG = "#1a1a2e";
const EMPTY_TEXT = "#555";
const PARTIAL_LABEL = "#ffb3b3";
// The legacy #ce-panel-info text color (viewer.html:933).
const INFO_FILL = "#aaa";

/** Derived worker lanes + ids for task resolution (K7). */
interface WorkerLaneData {
  lanes: Record<number, WorkerLane>;
  workerIds: number[];
}

/** The handle tracks.ts + the shell drive (mirrors SpansTrackController). */
export interface EventsTrackController {
  /** The full `.d9-track` row template for the events track (K1/K2/K5/K6). */
  rowTemplate(track: TrackSpec): TemplateResult;
  /** Size + draw the event canvas (K1/S4). Called from sizeTracks inside the
   *  shell's frame tick. */
  paint(
    canvas: HTMLCanvasElement,
    drawW: number,
    trackHeight: number,
    dpr: number,
    viewStart: number,
    viewEnd: number,
  ): void;
  /** Tear down the tooltip element. */
  dispose(): void;
}

export function createEventsTrack(store: ViewerStore): EventsTrackController {
  // ── Derived caches (F5) ────────────────────────────────────────────────
  // Trace-invariant event data: recomputed only when the trace slice changes.
  const eventData = store.derived(["trace"], (s) =>
    computeEventTrackData(s.trace.trace?.customEvents),
  );
  // Worker poll lanes, for resolving the task/poll a custom event ran in (K7).
  // Same trace-keyed invalidation.
  const workerLanes = store.derived(["trace"], (s) =>
    buildWorkerLaneData(s.trace.trace),
  );

  // Stable name -> color assignment for this track's lifetime (K1/K5). An
  // independent assigner from the spans track's, exactly like the legacy pair
  // of maps (_ceColorMap vs _spanColorMap).
  const colorOf = makeColorAssigner();

  // Per-event task resolver (K7), memoized in a WeakMap and rebuilt only when
  // the worker lanes change (legacy `_taskForEventCache`).
  let taskCache: { key: WorkerLaneData; fn: (ev: CustomTraceEvent) => number | null } | null =
    null;

  // Per-frame render-model memo, keyed on the derived-data reference (trace
  // identity) + its primitive inputs so a selection-only change (dimming)
  // reuses the cached geometry (S4) but a trace/viewport/filter change
  // recomputes it. NOT keyed on the selection - the draw applies the fade.
  let modelCache: { data: EventTrackData; key: string; model: EventRenderModel } | null =
    null;
  // The buckets last painted, for canvas hit-testing (K3 hover, K4 click).
  let lastHits: readonly EventDrawBucket[] = [];
  // One tooltip element for K3, created lazily.
  let tooltipEl: HTMLDivElement | null = null;
  let sizer: CanvasSizer<CanvasRenderingContext2D> | null = null;
  let sizerCanvas: HTMLCanvasElement | null = null;

  function state(): StoreState {
    return store.getState() as StoreState;
  }

  /** The memoized per-event task resolver bound to the current lanes (K7). */
  function taskResolver(): (ev: CustomTraceEvent) => number | null {
    const laneData = workerLanes();
    if (taskCache !== null && taskCache.key === laneData) return taskCache.fn;
    const memo = new WeakMap<CustomTraceEvent, number | null>();
    const fn = (ev: CustomTraceEvent): number | null => {
      const cached = memo.get(ev);
      if (cached !== undefined) return cached;
      const t = resolveTaskForEvent(ev, laneData.lanes, laneData.workerIds);
      memo.set(ev, t);
      return t;
    };
    taskCache = { key: laneData, fn };
    return fn;
  }

  function renderModel(
    data: EventTrackData,
    s: StoreState,
    drawW: number,
    canvasH: number,
  ): EventRenderModel {
    const selectedNames = s.uiPrefs.selectedEventNames;
    const key = [
      s.viewport.viewStart,
      s.viewport.viewEnd,
      drawW,
      canvasH,
      [...selectedNames].sort().join(","),
    ].join("|");
    if (modelCache !== null && modelCache.data === data && modelCache.key === key) {
      return modelCache.model;
    }
    const model = buildEventRenderModel({
      data,
      viewStart: s.viewport.viewStart,
      viewEnd: s.viewport.viewEnd,
      drawW,
      canvasH,
      selectedNames,
      taskOf: taskResolver(),
      colorOf,
    });
    modelCache = { data, key, model };
    return model;
  }

  // ── Store dispatch helpers ─────────────────────────────────────────────

  function toggleName(name: string): void {
    const cur = state().uiPrefs.selectedEventNames;
    const next = new Set(cur);
    if (next.has(name)) next.delete(name);
    else next.add(name);
    store.update("uiPrefs", { selectedEventNames: next });
  }
  function clearNames(): void {
    store.update("uiPrefs", { selectedEventNames: new Set<string>() });
  }

  /** Pin (or toggle off) a clicked cluster (K4); see dispatchEventPin. */
  function pinCluster(bucket: EventDrawBucket): void {
    const laneData = workerLanes();
    dispatchEventPin(store, bucket, laneData.lanes, laneData.workerIds);
  }

  /** Dispatch (or clear) the hover guide-line timestamp (I4; T24 draws it). */
  function setHoverEvent(ts: number | null): void {
    dispatchHoverEvent(store, ts);
  }

  // ── Canvas paint (K1/S4) ────────────────────────────────────────────────

  function paint(
    canvas: HTMLCanvasElement,
    drawW: number,
    trackHeight: number,
    dpr: number,
    _viewStart: number,
    _viewEnd: number,
  ): void {
    if (sizer === null || sizerCanvas !== canvas) {
      sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      sizerCanvas = canvas;
    }
    const canvasH = Math.max(0, trackHeight - LEGEND_H);
    const ctx = sizer.ensure(drawW, canvasH, dpr);
    const s = state();
    const data = eventData();
    const model = renderModel(data, s, drawW, canvasH);
    lastHits = model.buckets;
    const hlTask = eventHighlightTask(s.selection);
    const oversized = anyOversized(s.segments.segments);
    drawEventsCanvas(ctx, model, hlTask, drawW, canvasH, oversized, s.uiPrefs.selectedEventNames.size > 0);
    // Mirror the K2 info readout to a DOM-queryable attribute (the legacy
    // `#ce-panel-info` text) for the row-walker / behavioral differ - same as
    // the CPU track's `cpuReadout` (tracks.ts).
    canvas.dataset["eventsInfo"] = model.info;
  }

  // ── Templates (K5 legend chips, K6 clear) ───────────────────────────────

  function rowTemplate(track: TrackSpec): TemplateResult {
    const s = state();
    const data = eventData();
    const hasEvents = data.eventNames.length > 0;
    const selectedNames = s.uiPrefs.selectedEventNames;
    return html`
      <div
        class="d9-track d9-track--events"
        data-track-id=${track.id}
        style="height:${track.height}px"
      >
        <div class="d9-track-label d9-events-label" id="d9-track-label-events">
          <span class="d9-track-name">${track.label}</span>
        </div>
        <div class="d9-track-body d9-events-body">
          ${hasEvents ? legendTemplate(data, selectedNames) : nothing}
          <div class="d9-events-canvas-wrap">
            <canvas
              class="d9-track-canvas d9-events-canvas"
              data-track-canvas=${track.id}
              aria-labelledby="d9-track-label-events"
              role="img"
              @click=${onCanvasClick}
              @mousemove=${onCanvasHover}
              @mouseleave=${onCanvasLeave}
            ></canvas>
          </div>
        </div>
      </div>
    `;
  }

  /** K5 name-chip legend + K6 clear button (keyed template, not innerHTML). */
  function legendTemplate(
    data: EventTrackData,
    selectedNames: ReadonlySet<string>,
  ): TemplateResult {
    return html`
      <div class="d9-events-legend" role="group" aria-label="Event name filter">
        ${selectedNames.size > 0
          ? html`<button
              class="d9-event-btn"
              type="button"
              title="Clear event-name filter"
              @click=${clearNames}
            >
              ✕ Clear
            </button>`
          : nothing}
        <span class="d9-event-chips">
          ${repeat(
            data.eventNames,
            (name) => name,
            (name) => {
              const active = selectedNames.has(name);
              return html`
                <button
                  class=${classMap({ "d9-event-chip": true, on: active })}
                  type="button"
                  style="--chip:${colorOf(name)}"
                  aria-pressed=${active ? "true" : "false"}
                  @click=${() => toggleName(name)}
                >
                  ${name}
                </button>
              `;
            },
          )}
        </span>
      </div>
    `;
  }

  // ── Canvas interaction (K4 click, K3 hover) ─────────────────────────────

  function bucketAt(canvas: HTMLCanvasElement, ev: MouseEvent): EventDrawBucket | null {
    const rect = canvas.getBoundingClientRect();
    const mx = ev.clientX - rect.left;
    const my = ev.clientY - rect.top;
    for (let i = lastHits.length - 1; i >= 0; i--) {
      const h = lastHits[i]!;
      if (mx >= h.hitX && mx <= h.hitX + h.hitW && my >= h.y && my <= h.y + h.h) {
        return h;
      }
    }
    return null;
  }

  function onCanvasClick(ev: MouseEvent): void {
    const canvas = ev.currentTarget as HTMLCanvasElement;
    const bucket = bucketAt(canvas, ev);
    // Legacy: a click that misses every tick does nothing (no clear).
    if (bucket !== null) pinCluster(bucket);
  }

  function onCanvasHover(ev: MouseEvent): void {
    const canvas = ev.currentTarget as HTMLCanvasElement;
    const bucket = bucketAt(canvas, ev);
    canvas.style.cursor = bucket !== null ? "pointer" : "default";
    if (bucket === null) {
      hideTooltip();
      setHoverEvent(null);
      return;
    }
    showTooltip(ev, bucket);
    // Guide line across the lanes at the hovered event's timestamp (I4; the
    // T24 overlay draws it from this slice).
    setHoverEvent(bucket.representative.timestamp);
  }

  function onCanvasLeave(ev: MouseEvent): void {
    (ev.currentTarget as HTMLCanvasElement).style.cursor = "default";
    hideTooltip();
    setHoverEvent(null);
  }

  function tooltip(): HTMLDivElement {
    if (tooltipEl === null) {
      tooltipEl = document.createElement("div");
      tooltipEl.className = "d9-event-tooltip";
      tooltipEl.setAttribute("role", "tooltip");
      document.body.appendChild(tooltipEl);
    }
    return tooltipEl;
  }

  function showTooltip(ev: MouseEvent, bucket: EventDrawBucket): void {
    const el = tooltip();
    const axis = deriveAxisInputs(state());
    const fmtTs = (ns: number): string => fmtAxisTick(axis, ns, false);
    render(tooltipContent(bucket, fmtTs), el);
    el.style.display = "block";
    // Place near the cursor, flipping above/left if it would overflow.
    const pad = 12;
    const rect = el.getBoundingClientRect();
    let x = ev.clientX + pad;
    let y = ev.clientY + pad;
    if (x + rect.width > window.innerWidth) x = ev.clientX - rect.width - pad;
    if (y + rect.height > window.innerHeight) y = ev.clientY - rect.height - pad;
    el.style.left = `${Math.max(0, x)}px`;
    el.style.top = `${Math.max(0, y)}px`;
  }

  function hideTooltip(): void {
    if (tooltipEl !== null) tooltipEl.style.display = "none";
  }

  function tooltipContent(
    bucket: EventDrawBucket,
    fmtTs: (ns: number) => string,
  ): TemplateResult {
    const taskLine =
      bucket.taskId != null
        ? html`<div>
            <span class="tt-k">Task:</span> 0x${bucket.taskId.toString(16)}
            <span class="tt-dim">(click to select)</span>
          </div>`
        : html`<div><span class="tt-dim">(click to inspect)</span></div>`;
    if (bucket.events.length === 1) {
      const ev = bucket.representative;
      const fields = Object.entries(ev.fields ?? {});
      return html`
        <div><span class="tt-k">Event:</span> ${ev.name}</div>
        ${fields.map(
          ([k, v]) => html`<div>
            <span class="tt-k">${k}:</span> ${formatFieldValue(v, ev.units?.[k])}
          </div>`,
        )}
        <div><span class="tt-k">@</span> ${fmtTs(ev.timestamp)}</div>
        ${taskLine}
      `;
    }
    const events = bucket.events;
    const first = events[0]!;
    const last = events[events.length - 1]!;
    const rangeEnd =
      last.timestamp !== first.timestamp ? html` – ${fmtTs(last.timestamp)}` : nothing;
    return html`
      <div><span class="tt-k">Cluster:</span> ${events.length} events</div>
      <div><span class="tt-k">Types:</span> ${topNames(events)}</div>
      <div><span class="tt-k">@</span> ${fmtTs(first.timestamp)}${rangeEnd}</div>
      ${taskLine}
    `;
  }

  function dispose(): void {
    if (tooltipEl !== null) {
      tooltipEl.remove();
      tooltipEl = null;
    }
  }

  return { rowTemplate, paint, dispose };
}

// ── Store dispatch (exported so the DoD's dispatch checks are Vitest-able
//    without a DOM; the controller's canvas handlers delegate here) ─────────

/**
 * Pin a clicked cluster to the selection slice (K4), or toggle the pin off
 * when the SAME cluster is clicked again (legacy viewer.html:4292-4315). This
 * is the "dispatch side" the T27 ticket owns: it writes selection.pinnedEvent
 * (the contract T24's overlay marker + T31's inspector read) and clears any
 * prior task/span selection so the lanes show only this event's poll + marker
 * (legacy clearSpanTaskSelection). It does NOT draw the marker (T24 does).
 */
export function dispatchEventPin(
  store: ViewerStore,
  bucket: EventDrawBucket,
  lanes: Record<number, WorkerLane>,
  workerIds: readonly number[],
): void {
  const prior = (store.getState() as StoreState).selection.pinnedEvent;
  if (isSameCluster(prior, bucket)) {
    store.update("selection", { pinnedEvent: null });
    return;
  }
  store.update("selection", {
    pinnedEvent: buildPinnedEvent(bucket, lanes, workerIds),
    selectedTaskId: null,
    spanFocus: null,
    focusedSpanId: null,
  });
}

/**
 * Dispatch (or clear) the hovered custom-event timestamp for the guide line
 * (I4). T27 writes transient.hoverEventTs; the T24 overlay draws the orange
 * guide from it. Rides the transient channel, so it never redraws the full
 * track column. No-op when unchanged (avoids a redundant frame).
 */
export function dispatchHoverEvent(store: ViewerStore, ts: number | null): void {
  if ((store.getState() as StoreState).transient.hoverEventTs !== ts) {
    store.update("transient", { hoverEventTs: ts });
  }
}

// ── Pure helpers (no store) ──────────────────────────────────────────────

/** Build the worker poll lanes + ids for task resolution (K7). Empty when the
 *  trace has no events (mirrors spans-track's buildWorkerLanes). */
function buildWorkerLaneData(trace: ParsedTrace | null): WorkerLaneData {
  if (trace === null || trace.maxTs === null) return { lanes: {}, workerIds: [] };
  const workerIds = [...new Set(trace.tidToWorker.values())];
  const lanes = buildWorkerSpans(
    trace.events,
    workerIds,
    trace.maxTs,
    trace.blockInPlaceGaps,
  ).workerSpans;
  return { lanes, workerIds };
}

/** Most-frequent-first "name xN" list (legacy topNames), for cluster tooltips. */
function topNames(events: readonly CustomTraceEvent[], limit = 5): string {
  const counts = new Map<string, number>();
  for (const e of events) counts.set(e.name, (counts.get(e.name) ?? 0) + 1);
  return [...counts.entries()]
    .sort((a, b) => b[1] - a[1])
    .slice(0, limit)
    .map(([n, c]) => (c > 1 ? `${n} ×${c}` : n))
    .join(", ");
}

/** True when any segment is stuck "oversized" (T17 obligation): the view is
 *  unavoidably partial. Empty on the whole-trace path. */
function anyOversized(segments: ReadonlyMap<string, { state: string }>): boolean {
  for (const entry of segments.values()) {
    if (entry.state === "oversized") return true;
  }
  return false;
}

// ── Canvas draw (extracted so the render input is unit-testable) ──────────

/**
 * Draw the event marker ticks into `ctx` (already DPR-scaled + sized to
 * drawW x canvasH). Ported from the legacy `renderCustomEventsPanel`
 * (viewer.html:3588-3637): each cluster is a colored tick whose alpha encodes
 * cluster size, faded to 20% when a task is highlighted and this cluster did
 * NOT run on it (the S4/K1 selection-driven dimming - a cheap redraw over the
 * cached geometry, mirroring T26's spans dimming). Mixed-name clusters get a
 * secondary-color bottom stripe. `hlTask` is the highlighted task (null = no
 * dimming); `hasNameFilter` only tunes the empty-state message.
 */
export function drawEventsCanvas(
  ctx: CanvasRenderingContext2D,
  model: EventRenderModel,
  hlTask: number | null,
  drawW: number,
  canvasH: number,
  oversized: boolean,
  hasNameFilter: boolean,
): void {
  ctx.clearRect(0, 0, drawW, canvasH);
  ctx.fillStyle = CANVAS_BG;
  ctx.fillRect(0, 0, drawW, canvasH);
  if (drawW <= 0 || canvasH <= 0) return;

  if (model.emptyReason !== null) {
    ctx.fillStyle = EMPTY_TEXT;
    ctx.font = "12px sans-serif";
    ctx.textAlign = "left";
    const msg =
      model.emptyReason === "no-events"
        ? "No custom events"
        : "No events in view" + (hasNameFilter ? " (name filter active)" : "");
    ctx.fillText(msg, 10, Math.min(24, canvasH / 2));
    if (oversized) drawPartialBadge(ctx, drawW, canvasH);
    return;
  }

  for (const bucket of model.buckets) {
    let alpha = bucket.baseAlpha;
    if (hlTask != null && bucket.taskId !== hlTask) alpha *= EVENT_DIM_FACTOR;
    const left = bucket.px - bucket.w / 2;

    ctx.fillStyle = bucket.color;
    ctx.globalAlpha = alpha;
    ctx.fillRect(left, bucket.y, bucket.w, bucket.h);

    if (bucket.secondColor !== null) {
      ctx.fillStyle = bucket.secondColor;
      ctx.fillRect(left, bucket.y + bucket.h - 4, bucket.w, 4);
    }
  }
  ctx.globalAlpha = 1.0;

  // K2 info readout, top-right (legacy `#ce-panel-info`: `N events · M
  // markers`, overlaid on the panel).
  if (model.info.length > 0) {
    ctx.fillStyle = INFO_FILL;
    ctx.font = "10px sans-serif";
    ctx.textAlign = "right";
    ctx.fillText(model.info, Math.max(0, drawW - 6), 11);
    ctx.textAlign = "left";
  }

  // T17 obligation: surface a permanently-partial (oversized) window.
  if (oversized) drawPartialBadge(ctx, drawW, canvasH);
}

/** The "partial window" badge (T17 obligation), mirroring the CPU track. */
function drawPartialBadge(
  ctx: CanvasRenderingContext2D,
  drawW: number,
  canvasH: number,
): void {
  ctx.globalAlpha = 1.0;
  ctx.fillStyle = PARTIAL_LABEL;
  ctx.font = "10px monospace";
  ctx.textAlign = "right";
  ctx.fillText("partial window", Math.max(0, drawW - 3), canvasH - 3);
  ctx.textAlign = "left";
}
