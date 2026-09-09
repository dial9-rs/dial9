// Heatmap pointer interaction: plain drag selects a region, Option/Alt+drag
// zooms the time axis, click selects one segment, double-click resets zoom,
// click-outside clears the selection, and window resizes trigger a
// debounced repaint. Raw events translate into store actions and
// transient-channel updates only; rendering is the selection overlay's and
// painter's job.
//
// The clear-on-click-away decision itself lives in the frozen core
// (shouldClearSelectionOnClick), so the set of control surfaces that must
// NOT clear a selection is stated once and unit-tested there.

import { shouldClearSelectionOnClick } from "../../lib/canvas/heatmap.js";
import { DRAG_INTENT_PX } from "../../lib/interact/pointer.js";
import { ROW_H } from "./actions.js";
import type { PageCtx } from "./ctx.js";
import { clamp } from "./format.js";
import { cursorPlacement, timestampAt } from "./heatmap-axis.js";

export function mountHeatmapInteraction({ store, els, actions }: PageCtx): void {
  let dragging = false;
  let zooming = false;
  let startX = 0;
  let startY = 0;
  // A drag ending outside the plot emits a trailing click on an ancestor.
  // Consume that click so it cannot clear the selection committed on mouseup.
  let justDraggedOnHeatmap = false;

  function localXY(e: MouseEvent): { x: number; y: number } {
    const rect = els.heatmapPlot.getBoundingClientRect();
    const H = store.getState().browse.rows.length * ROW_H;
    // Clamp x to the canvas width (what the time<->x helpers map against),
    // not the plot's bounding rect, so the far-right edge lines up exactly.
    const W = els.heatmapCanvas.clientWidth || rect.width;
    return {
      x: clamp(e.clientX - rect.left, 0, W),
      y: clamp(e.clientY - rect.top, 0, H),
    };
  }

  els.heatmapPlot.addEventListener("mousedown", (e) => {
    if (!store.getState().browse.rows.length) return;
    justDraggedOnHeatmap = false;
    dragging = true;
    hideCursor();
    zooming = e.altKey;
    const { x, y } = localXY(e);
    startX = x;
    startY = y;
    store.update("transient", { drag: { x0: x, y0: y, x1: x, y1: y, zooming } });
    e.preventDefault();
  });

  window.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    const { x, y } = localXY(e);
    store.update("transient", {
      drag: { x0: startX, y0: startY, x1: x, y1: y, zooming },
    });
  });

  window.addEventListener("mouseup", (e) => {
    if (!dragging) return;
    dragging = false;
    const { x, y } = localXY(e);
    store.update("transient", { drag: null });
    if (zooming) {
      zooming = false;
      justDraggedOnHeatmap = true;
      actions.zoomToX(Math.min(x, startX), Math.max(x, startX));
      return;
    }
    const dx = Math.abs(x - startX);
    const dy = Math.abs(y - startY);
    if (dx <= DRAG_INTENT_PX && dy <= DRAG_INTENT_PX) {
      actions.selectSegmentAt(startX, startY); // treat as a click
    } else {
      justDraggedOnHeatmap = true;
      actions.finalizeSelection(
        Math.min(x, startX),
        Math.max(x, startX),
        Math.min(y, startY),
        Math.max(y, startY),
      );
    }
  });

  // Hover readout: a thin cursor line follows the pointer with a label
  // showing the timestamp beneath it, so "what time am I looking at?" is
  // answerable without dragging out a selection to read its bounds.
  // Suppressed mid-drag - the rubber band already communicates that span -
  // and whenever there is no data to point at.
  function hideCursor(): void {
    els.heatmapCursor.style.display = "none";
    els.heatmapCursorLabel.style.display = "none";
  }

  els.heatmapPlot.addEventListener("mousemove", (e) => {
    const s = store.getState();
    const domain = s.browse.domain;
    if (dragging || !s.browse.rows.length || !domain) {
      hideCursor();
      return;
    }
    const { x } = localXY(e);
    const W =
      els.heatmapCanvas.clientWidth || parseFloat(els.heatmapCanvas.style.width) || 1;
    // Write the text first: the placement clamp measures the label, and a
    // date-carrying timestamp is materially wider than a bare time.
    els.heatmapCursorLabel.textContent = timestampAt(x, domain, W, s.ui.useLocalTz);
    // display:none elements measure zero, including the first hover after leaving.
    els.heatmapCursorLabel.style.display = "block";
    const place = cursorPlacement(x, W, els.heatmapCursorLabel.offsetWidth);
    els.heatmapCursor.style.left = place.lineLeft + "px";
    els.heatmapCursor.style.height = s.browse.rows.length * ROW_H + "px";
    els.heatmapCursor.style.display = "block";
    els.heatmapCursorLabel.style.left = place.labelLeft + "px";
  });
  els.heatmapPlot.addEventListener("mouseleave", hideCursor);

  // Double-click anywhere on the plot resets to the full time range.
  els.heatmapPlot.addEventListener("dblclick", () => {
    actions.resetHeatmapZoom();
  });

  // The "Reset zoom" button in the hint bar.
  els.heatmapResetZoom.addEventListener("click", () => {
    actions.resetHeatmapZoom();
  });

  // Clicking away from the timeline clears the current selection. Clicks
  // inside the plot are handled by its own mousedown/up; the actions bar and
  // the page header are control surfaces, so clicking a button there (Open,
  // Flamegraph, the TZ toggle, AWS Credentials) must preserve the selection -
  // toggling TZ only relabels the axis (#645).
  document.addEventListener("click", (e) => {
    const wasDrag = justDraggedOnHeatmap;
    justDraggedOnHeatmap = false;
    const s = store.getState();
    const target = e.target instanceof Element ? e.target : null;
    const inside = (selector: string): boolean => !!target?.closest(selector);
    if (
      shouldClearSelectionOnClick({
        isBrowseTab: s.ui.tab === "browse",
        hasSelection: !!s.browse.selection,
        wasDrag,
        targetInHeatmap: inside("#heatmap-view"),
        targetInActions: inside("#actions-bar"),
        targetInHeader: inside("header"),
      })
    ) {
      actions.setHeatmapSelection(null);
    }
  });

  // Redraw the canvas (and re-measure width) on window resize, debounced
  // 100ms. The renderEpoch bump repaints the canvas and re-places the
  // selection rect through the normal render subscriptions.
  let resizeTimer: ReturnType<typeof setTimeout> | null = null;
  window.addEventListener("resize", () => {
    const s = store.getState();
    if (s.ui.tab !== "browse" || !s.browse.rows.length) return;
    if (resizeTimer !== null) clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      store.update("browse", {
        renderEpoch: store.getState().browse.renderEpoch + 1,
      });
    }, 100);
  });
}
