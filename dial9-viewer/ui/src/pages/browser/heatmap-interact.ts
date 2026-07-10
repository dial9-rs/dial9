// Heatmap pointer interaction (features/01 F12-F15, F18, F19): plain drag
// selects a region, Option/Alt+drag zooms the time axis, click selects one
// segment, double-click resets zoom, click-outside clears the selection,
// and window resizes trigger a debounced repaint. Ported from the legacy
// setupHeatmapInteraction IIFE (index.html:1460-1523) + the resize and
// click-outside listeners. Raw events translate into store actions and
// transient-channel updates only (architecture 2.5); rendering is the
// selection overlay's and painter's job.

import { ROW_H } from "./actions.js";
import type { PageCtx } from "./ctx.js";
import { clamp } from "./format.js";

export function mountHeatmapInteraction({ store, els, actions }: PageCtx): void {
  let dragging = false;
  let zooming = false;
  let startX = 0;
  let startY = 0;

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
    dragging = true;
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
      actions.zoomToX(Math.min(x, startX), Math.max(x, startX));
      return;
    }
    const dx = Math.abs(x - startX);
    const dy = Math.abs(y - startY);
    if (dx < 4 && dy < 4) {
      actions.selectSegmentAt(startX, startY); // treat as a click
    } else {
      actions.finalizeSelection(
        Math.min(x, startX),
        Math.max(x, startX),
        Math.min(y, startY),
        Math.max(y, startY),
      );
    }
  });

  // Double-click anywhere on the plot resets to the full time range (F15).
  els.heatmapPlot.addEventListener("dblclick", () => {
    actions.resetHeatmapZoom();
  });

  // The "Reset zoom" button in the hint bar (F16; legacy inline onclick).
  els.heatmapResetZoom.addEventListener("click", () => {
    actions.resetHeatmapZoom();
  });

  // Clicking anywhere outside the timeline and the actions bar clears the
  // current selection (F18). Clicks inside the plot are handled by its own
  // mousedown/up; clicks on the action buttons must preserve the selection.
  document.addEventListener("click", (e) => {
    const s = store.getState();
    if (s.ui.tab !== "browse" || !s.browse.selection) return;
    const target = e.target instanceof Element ? e.target : null;
    if (target && (target.closest("#heatmap-view") || target.closest("#actions-bar"))) {
      return;
    }
    actions.setHeatmapSelection(null);
  });

  // Redraw the canvas (and re-measure width) on window resize, debounced
  // 100ms (F19). The renderEpoch bump repaints the canvas and re-places the
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
