// The transient overlay controller: wires the crosshair canvas, hover tooltip,
// and at-cursor readout to the viewer store's `transient` channel. This is the
// ONE place the hover path lives:
//
//   pointer mousemove -> store.update("transient", {mouseNs, atCursor}) (reads
//     column/lane geometry ONCE at the start of a fresh event - a clean read,
//     never after a write - then dispatches; no synchronous DOM write here)
//   store transient tick -> drawFrame (RAF-coalesced): draw the crosshair
//     overlay, render the tooltip from CACHED dims, mirror the readout. On a
//     hover-only frame the shell does NOT run (it never subscribes to
//     `transient`), so drawFrame is the sole subscriber and its geometry reads
//     precede all its writes: ZERO forced synchronous layout.
//
// This controller owns the HOVER path only (mouseNs + tooltip + readout).
// Drag/zoom gestures and the keyboard-selection cursor populate
// `transient.drag` / `transient.keyboardSelection`, which the crosshair reads.
// When a drag is active this controller suppresses the tooltip + mouse
// crosshair, so the two never fight.

import type { ViewerStore } from "../../store/store.js";
import type { StoreState, TimePanelLayout } from "../../types/state.js";
import { createCanvasSizer, type CanvasSizer } from "../../lib/canvas/dpr.js";
import { LABEL_W, laneRowLayout, timePanelLayout, workerAtLaneY } from "../../lib/canvas/layout.js";
import { LANE_ROW_H, RUNTIME_HEADER_H } from "../canvas/lanes/render.js";
import { lanesScrollbarWidth } from "../../lib/canvas/track-layout.js";
import { assembleLaneHover } from "../canvas/lanes/index.js";
import type { LaneHoverInput } from "../canvas/lanes/hover.js";
import { deriveOverlayData, type OverlayData } from "./data.js";
import { drawCrosshair, type CrosshairContext } from "./crosshair.js";
import { computeAtCursorReadout, coverageAt } from "./readout.js";
import { buildLaneTooltip, createTooltip, type TooltipHandle } from "./tooltip.js";

const OVERLAY_CLASS = "d9-crosshair-overlay";

export interface MountedOverlay {
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
  const scrollbarW = lanesScrollbarWidth(trackColumn);
  // Height must cover the tracks so the crosshair spans them even when scrolled,
  // but must NOT read trackColumn.scrollHeight: this overlay is an absolutely
  // positioned child, so it contributes to scrollHeight itself - sizing to
  // scrollHeight is a feedback ratchet that only ever grows the column's scroll
  // area (phantom empty scroll below the last track). Measure the real FLOW
  // content instead (hint chips + tracks), floored at the visible height.
  const flowContent =
    trackColumn.querySelector<HTMLElement>(".d9-tracks") ?? trackColumn;
  const hint = trackColumn.querySelector<HTMLElement>(".d9-hint-chips");
  const contentHeight =
    (hint?.offsetHeight ?? 0) + flowContent.offsetHeight;
  return {
    layout: timePanelLayout({ pw, scrollbarW, viewStart, viewEnd }),
    pw,
    scrollHeight: Math.max(trackColumn.clientHeight, contentHeight),
  };
}

/** Worker id under `clientY` in the lanes box, or null when off the lanes / over
 *  a runtime header. Fixed-height rows + box scrollTop, resolved through the same
 *  shared row layout the renderer + click use so hover never drifts from them. */
function workerAtClientY(
  box: HTMLElement,
  clientY: number,
  data: OverlayData,
  collapsedRuntimes: Readonly<Record<string, boolean>>,
): number | null {
  if (data.workerIds.length === 0) return null;
  const rect = box.getBoundingClientRect();
  if (clientY < rect.top || clientY >= rect.bottom || rect.height <= 0) return null;
  const localY = clientY - rect.top + box.scrollTop;
  const rowLayout = laneRowLayout(
    data.runtimeGroups,
    LANE_ROW_H,
    RUNTIME_HEADER_H,
    collapsedRuntimes,
  );
  return workerAtLaneY(rowLayout, localY);
}

/**
 * Mount the transient overlay against `store`, drawing over the shell's track
 * column. `root` is the viewer app root (the readout stub mirrors into its
 * inspector slot). Returns teardown handles.
 */
/**
 * How the host page renders a timestamp. Injected rather than imported so this
 * component does not reach up into a page: the axis's absolute/relative clock
 * mode and its date qualification are viewer policy, not overlay policy.
 */
export type FormatTimestamp = (state: StoreState, ns: number) => string;

export function mountOverlay(
  root: HTMLElement,
  trackColumn: HTMLElement,
  store: ViewerStore,
  formatTimestamp: FormatTimestamp,
): MountedOverlay {
  // Frame-invariant hover data, recomputed only when the trace slice is
  // replaced. Null until a trace loads.
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
      // Non-interactive overlay: clicks fall through to the lanes.
      trackColumn.appendChild(canvas);
    }
    if (canvas !== overlayCanvas) {
      sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      overlayCanvas = canvas;
    }
    return canvas;
  }

  function drawFrame(): void {
    const state = store.getState();
    ensureOverlay();
    const { viewStart, viewEnd } = state.viewport;

    // ALL reads first (geometry), THEN all writes: on a hover-only frame this
    // subscriber is alone, so its reads precede every write in the frame and
    // force no layout.
    const geom = columnGeometry(trackColumn, viewStart, viewEnd);
    const dpr = (typeof devicePixelRatio === "number" ? devicePixelRatio : 1) || 1;

    const formatTs = (ns: number): string => formatTimestamp(state, ns);

    // writes from here down
    if (sizer === null) return;
    const ctx = sizer.ensure(geom.pw, geom.scrollHeight, dpr);

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

    // At-cursor readout: the overlay's job is to POPULATE `transient.atCursor`
    // (computeAtCursorReadout); the persistent inspector RENDERS it in one
    // place, so nothing renders it here.

    // Tooltip: rebuilt only on a fresh mousemove; hidden on mouseleave/drag.
    // Uses CACHED dims (no measure-after-write).
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
            columnarSpans: data.columnarSpans,
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

  // Pointer wiring (hover only).

  function onMouseMove(e: MouseEvent): void {
    const state = store.getState();
    // Suppress the tooltip + mouse crosshair while a drag is in flight.
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
    const lanesBox = trackColumn.querySelector<HTMLElement>(".d9-lanes-viewport");
    const workerId = lanesBox
      ? workerAtClientY(lanesBox, e.clientY, data, state.uiPrefs.collapsedRuntimes)
      : null;

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
    const state = store.getState();
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
export { computeAtCursorReadout, coverageAt } from "./readout.js";
export type { AtCursorInput } from "./readout.js";
export { deriveOverlayData } from "./data.js";
export type { OverlayData } from "./data.js";
