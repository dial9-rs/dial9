// Keyboard window selection on the browse heatmap. The state machine:
//   - focus the plot (now tabbable), press Shift: a selection window
//     starts at the center of the visible time range;
//   - ArrowLeft / ArrowRight move its leading edge (5% steps);
//   - Enter (or Shift again) confirms: the same finalizeSelection the
//     mouse path uses (all host rows - the keyboard window selects
//     across hosts; row narrowing stays a pointer affordance);
//   - Escape cancels.
// The in-progress window renders through the SAME transient drag channel
// as the mouse rubber band (selection-overlay.ts), so the keyboard path
// gets the identical visual for free.
//
// Raw key events translate into store actions and transient-channel
// updates only; rendering stays with the overlay.

import { getAnnouncer } from "../../lib/interact/index.js";
import { ROW_H } from "./actions.js";
import type { PageCtx } from "./ctx.js";
import { clamp } from "./format.js";

// Edge step per arrow press, as a fraction of the plot width.
const STEP = 0.05;

export function mountHeatmapKeys({ store, els, actions }: PageCtx): void {
  const announcer = getAnnouncer();

  // The scrollable plot becomes keyboard-focusable.
  // role=application: arrow keys are the widget's, not AT navigation.
  els.heatmapPlot.tabIndex = 0;
  els.heatmapPlot.setAttribute("role", "application");
  els.heatmapPlot.setAttribute(
    "aria-label",
    "Trace heatmap. Press Shift to start a window selection, " +
      "arrow keys to extend, Enter to confirm, Escape to cancel.",
  );
  els.heatmapPlot.classList.add("d9-kb-focus");

  let selecting = false;
  let anchorX = 0; // px, plot-local
  let cursorX = 0;

  function plotWidth(): number {
    return els.heatmapCanvas.clientWidth || 1;
  }

  function rowsHeight(): number {
    return store.getState().browse.rows.length * ROW_H;
  }

  function paint(): void {
    store.update("transient", {
      drag: { x0: anchorX, y0: 0, x1: cursorX, y1: rowsHeight(), zooming: false },
    });
  }

  function cancel(announce: boolean): void {
    if (!selecting) return;
    selecting = false;
    store.update("transient", { drag: null });
    if (announce) announcer.announce("Selection cancelled");
  }

  function start(): void {
    const W = plotWidth();
    // Start as a centered one-step window so the band is visible
    // immediately (a zero-width band renders as a hairline).
    anchorX = W * (0.5 - STEP / 2);
    cursorX = W * (0.5 + STEP / 2);
    selecting = true;
    paint();
    announcer.announce(
      "Selection started: arrows extend, Shift or Enter confirms, Escape cancels",
    );
  }

  function confirm(): void {
    selecting = false;
    store.update("transient", { drag: null });
    actions.finalizeSelection(
      Math.min(anchorX, cursorX),
      Math.max(anchorX, cursorX),
      0,
      rowsHeight(),
    );
    const sel = store.getState().browse.selection;
    announcer.announce(
      sel !== null && sel.keys.length > 0
        ? `Selected ${sel.keys.length} segment${sel.keys.length === 1 ? "" : "s"}`
        : "No segments in the selected window",
    );
  }

  els.heatmapPlot.addEventListener("keydown", (e) => {
    // Untouched modifiers/chords pass through (browser zoom etc.).
    if (e.ctrlKey || e.metaKey || e.altKey) return;

    if (e.key === "Shift") {
      if (e.repeat) return; // held Shift must not toggle start/confirm
      if (!store.getState().browse.rows.length) return;
      e.preventDefault();
      if (!selecting) start();
      else confirm();
      return;
    }
    if (!selecting) return; // arrows/Enter/Escape only act mid-selection

    if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
      e.preventDefault();
      const W = plotWidth();
      const step = W * STEP;
      cursorX = clamp(cursorX + (e.key === "ArrowLeft" ? -step : step), 0, W);
      paint();
    } else if (e.key === "Enter") {
      e.preventDefault();
      confirm();
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancel(true);
    }
  });

  // Leaving the plot abandons an unconfirmed window (matches the mouse
  // rubber band, which cannot outlive its drag).
  els.heatmapPlot.addEventListener("blur", () => {
    cancel(false);
  });
}
