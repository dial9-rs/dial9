// components/overlay/index.ts - the transient overlay controller (T24): wires
// the crosshair canvas, hover tooltip, and at-cursor readout to the viewer
// store's `transient` channel. This is the ONE place the hover path lives:
//
//   pointer mousemove ─▶ store.update("transient", {mouseNs, atCursor})  (reads
//     column/lane geometry ONCE at the start of a fresh event - a clean read,
//     never after a write - then dispatches; no synchronous DOM write here)
//   store transient tick ─▶ drawFrame (RAF-coalesced): draw the crosshair
//     overlay, render the tooltip from CACHED dims, mirror the readout. On a
//     hover-only frame the shell does NOT run (it never subscribes to
//     `transient`), so drawFrame is the sole subscriber and its geometry reads
//     precede all its writes: ZERO forced synchronous layout (the 03 F3 fix).
//
// SEAM with T23 (pointer/keyboard state machine, not yet landed): T24 owns the
// HOVER path only (mouseNs + tooltip + readout). Drag/zoom gestures and the
// keyboard-selection cursor are T23's - they populate `transient.drag` /
// `transient.keyboardSelection`, which the crosshair already reads. When a
// drag is active this controller suppresses the tooltip + mouse crosshair
// (legacy V3), so the two never fight.

import type { ViewerStore } from "../../store/store.js";
import type { StoreState, TimePanelLayout } from "../../types/state.js";
import { createCanvasSizer, type CanvasSizer } from "../../lib/canvas/dpr.js";
import { LABEL_W, timePanelLayout } from "../../lib/canvas/layout.js";
import { deriveAxisInputs, fmtAxisTick } from "../../pages/viewer/axis.js";
import { assembleLaneHover } from "../canvas/lanes/index.js";
import type { LaneHoverInput } from "../canvas/lanes/hover.js";
import { deriveOverlayData, type OverlayData } from "./data.js";
import { drawCrosshair, type CrosshairContext } from "./crosshair.js";
import { computeAtCursorReadout, coverageAt } from "./readout.js";
import { buildLaneTooltip, createTooltip, type TooltipHandle } from "./tooltip.js";

const OVERLAY_CLASS = "d9-crosshair-overlay";

export interface MountedOverlay {
  /** Force one overlay redraw (after mount / on resize). */
  refresh(): void;
  /** Tear down subscriptions, listeners, and the overlay DOM. */
  dispose(): void;
}

interface ColumnGeometry {
  layout: TimePanelLayout;
  /** Overlay CSS width (column client width). */
  pw: number;
  /** Overlay CSS height (full scrollable content height). */
  scrollHeight: number;
}

/** Read the shared column geometry ONCE (the lanes use the identical inputs). */
function columnGeometry(
  trackColumn: HTMLElement,
  viewStart: number,
  viewEnd: number,
): ColumnGeometry {
  const pw = trackColumn.clientWidth;
  const scrollbarW = Math.max(0, trackColumn.offsetWidth - trackColumn.clientWidth);
  return {
    layout: timePanelLayout({ pw, scrollbarW, viewStart, viewEnd }),
    pw,
    scrollHeight: trackColumn.scrollHeight,
  };
}

/** Worker id under `clientY` on the lanes track, or null when off the lanes. */
function workerAtClientY(
  lanesCanvas: HTMLElement,
  clientY: number,
  workerIds: readonly number[],
): number | null {
  const n = workerIds.length;
  if (n === 0) return null;
  const rect = lanesCanvas.getBoundingClientRect();
  if (clientY < rect.top || clientY >= rect.bottom || rect.height <= 0) return null;
  const idx = Math.floor(((clientY - rect.top) / rect.height) * n);
  return workerIds[idx] ?? null;
}

/**
 * Mount the transient overlay against `store`, drawing over the shell's track
 * column. `root` is the viewer app root (the readout stub mirrors into its
 * inspector slot). Returns teardown handles.
 */
export function mountOverlay(
  root: HTMLElement,
  trackColumn: HTMLElement,
  store: ViewerStore,
): MountedOverlay {
  // Frame-invariant hover data, recomputed only when the trace slice is
  // replaced (03 F5). Null until a trace loads.
  const overlayData = store.derived(["trace"], (s): OverlayData | null =>
    s.trace.trace ? deriveOverlayData(s.trace.trace) : null,
  );

  const tooltip: TooltipHandle = createTooltip(root.ownerDocument);

  let sizer: CanvasSizer<CanvasRenderingContext2D> | null = null;
  let overlayCanvas: HTMLCanvasElement | null = null;

  // Consumed-once tooltip request set by mousemove; a pan/select frame leaves
  // it null so the tooltip is not rebuilt (its content is unchanged at the
  // same ns). Hide is a separate one-shot flag (mouseleave / drag).
  let pendingTooltip: { cursor: { clientX: number; clientY: number }; workerId: number; ns: number } | null = null;
  let hidePending = false;

  /** Create the overlay canvas once and (idempotently) keep it attached. */
  function ensureOverlay(): HTMLCanvasElement {
    let canvas = trackColumn.querySelector<HTMLCanvasElement>(`.${OVERLAY_CLASS}`);
    if (!canvas) {
      canvas = root.ownerDocument.createElement("canvas");
      canvas.className = OVERLAY_CLASS;
      canvas.setAttribute("aria-hidden", "true");
      // Non-interactive overlay (I1): clicks fall through to the lanes.
      trackColumn.appendChild(canvas);
    }
    if (canvas !== overlayCanvas) {
      sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      overlayCanvas = canvas;
    }
    return canvas;
  }

  function drawFrame(): void {
    const state = store.getState() as StoreState;
    ensureOverlay();
    const { viewStart, viewEnd } = state.viewport;

    // ALL reads first (geometry), THEN all writes: on a hover-only frame this
    // subscriber is alone, so its reads precede every write in the frame and
    // force no layout (03 F3).
    const geom = columnGeometry(trackColumn, viewStart, viewEnd);
    const dpr = (typeof devicePixelRatio === "number" ? devicePixelRatio : 1) || 1;

    const axis = deriveAxisInputs(state);
    const formatTs = (ns: number): string => fmtAxisTick(axis, ns, false);

    // ── writes from here down ───────────────────────────────────────────
    if (sizer === null) return;
    const ctx = sizer.ensure(geom.pw, geom.scrollHeight, dpr) as unknown as CrosshairContext;

    const pinned = state.selection.pinnedEvent;
    drawCrosshair(ctx, {
      time: geom.layout,
      width: geom.pw,
      height: geom.scrollHeight,
      viewStart,
      viewEnd,
      mouseNs: state.transient.mouseNs,
      keyboardSelection: state.transient.keyboardSelection,
      hoverEventTs: state.transient.hoverEventTs,
      pinnedEvent: pinned
        ? { timestamp: pinned.timestamp, label: `${pinned.name} @ ${formatTs(pinned.timestamp)}` }
        : null,
    });

    // At-cursor readout (the S4 contract): the overlay's job is to POPULATE
    // `transient.atCursor` (computeAtCursorReadout, above); T31's persistent
    // inspector RENDERS it in one place, so the old inspector-slot stub no
    // longer runs here. renderAtCursorStub stays exported for its own test.

    // Tooltip (V-section): rebuilt only on a fresh mousemove; hidden on
    // mouseleave/drag. Uses CACHED dims (no measure-after-write).
    if (hidePending) {
      tooltip.hide();
      hidePending = false;
    } else if (pendingTooltip !== null) {
      const data = overlayData();
      if (data) {
        const req = pendingTooltip;
        const spans = data.workerSpans[req.workerId];
        if (spans) {
          const input: LaneHoverInput = {
            workerId: req.workerId,
            ns: req.ns,
            spans,
            allSpans: data.allSpans,
            queueSamples: data.queueSamples,
            localQueueSamples: data.workerQueueSamples[req.workerId] ?? [],
            activeTaskSamples: data.activeTaskSamples,
            blockInPlaceGaps: data.blockInPlaceGaps,
            hasCpuTime: data.hasCpuTime,
            hasSchedWait: data.hasSchedWait,
            hasTaskTracking: data.hasTaskTracking,
          };
          const hover = assembleLaneHover(input);
          const coverage = coverageAt(state.segments.segments, req.ns);
          tooltip.show(buildLaneTooltip(hover, { formatTs, coverage }), req.cursor, 130);
        } else {
          tooltip.hide();
        }
      }
      pendingTooltip = null;
    }
  }

  // ── Pointer wiring (hover only; drag/zoom are T23's) ────────────────────

  function onMouseMove(e: MouseEvent): void {
    const state = store.getState() as StoreState;
    // V3: suppress the tooltip + mouse crosshair while a drag is in flight.
    if (state.transient.drag !== null) {
      if (state.transient.mouseNs !== null) store.update("transient", { mouseNs: null, atCursor: null });
      pendingTooltip = null;
      hidePending = true;
      return;
    }
    const data = overlayData();
    if (data === null) return;

    const { viewStart, viewEnd } = state.viewport;
    // Clean reads (fresh event, layout committed): column geometry + ns.
    const geom = columnGeometry(trackColumn, viewStart, viewEnd);
    const rect = trackColumn.getBoundingClientRect();
    const mouseX = e.clientX - rect.left;
    const drawW = geom.layout.drawW;
    // Outside the draw area (label gutter, past drawW, degenerate view): clear.
    if (mouseX < LABEL_W || mouseX > LABEL_W + drawW || viewEnd <= viewStart) {
      if (state.transient.mouseNs !== null || state.transient.atCursor !== null) {
        store.update("transient", { mouseNs: null, atCursor: null });
      }
      pendingTooltip = null;
      hidePending = true;
      return;
    }

    const ns = geom.layout.panelXToNs(mouseX);
    const lanesCanvas = trackColumn.querySelector<HTMLElement>('canvas[data-track-canvas="lanes"]');
    const workerId = lanesCanvas ? workerAtClientY(lanesCanvas, e.clientY, data.workerIds) : null;

    const readout = computeAtCursorReadout(
      data,
      ns,
      workerId,
      coverageAt(state.segments.segments, ns),
    );
    store.update("transient", { mouseNs: ns, atCursor: readout });

    // Only queue the rich lane tooltip when the cursor is over a worker row.
    if (workerId !== null) {
      pendingTooltip = { cursor: { clientX: e.clientX, clientY: e.clientY }, workerId, ns };
      hidePending = false;
    } else {
      pendingTooltip = null;
      hidePending = true;
    }
  }

  function onMouseLeave(): void {
    const state = store.getState() as StoreState;
    if (state.transient.mouseNs !== null || state.transient.atCursor !== null) {
      store.update("transient", { mouseNs: null, atCursor: null });
    }
    pendingTooltip = null;
    hidePending = true;
  }

  trackColumn.addEventListener("mousemove", onMouseMove);
  trackColumn.addEventListener("mouseleave", onMouseLeave);

  // The overlay's own RAF channel: notified for transient (hover), viewport
  // (pan/zoom moves the line), selection (pinned marker), and trace (reload).
  const unsubscribe = store.subscribe(["transient", "viewport", "selection", "trace"], () =>
    drawFrame(),
  );

  // First paint if a trace is already resident.
  drawFrame();

  return {
    refresh: () => store.update("transient", {}),
    dispose(): void {
      unsubscribe();
      trackColumn.removeEventListener("mousemove", onMouseMove);
      trackColumn.removeEventListener("mouseleave", onMouseLeave);
      tooltip.dispose();
      overlayCanvas?.remove();
    },
  };
}

export { drawCrosshair, crosshairX } from "./crosshair.js";
export type { CrosshairInput, CrosshairContext } from "./crosshair.js";
export {
  placeTooltip,
  laneTooltipModel,
  buildLaneTooltip,
  createTooltip,
} from "./tooltip.js";
export type { TooltipHandle, TooltipRow, TooltipSegment } from "./tooltip.js";
export { computeAtCursorReadout, coverageAt, renderAtCursorStub } from "./readout.js";
export type { AtCursorInput } from "./readout.js";
export { deriveOverlayData } from "./data.js";
export type { OverlayData } from "./data.js";
