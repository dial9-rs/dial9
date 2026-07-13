// The viewport zoom/pan/fit actions and the `z` zoom-history integration.
//
// Two layers, kept apart so the arithmetic is testable without a store:
//
//   1. Pure math over a ViewportSlice (zoomWindow / panWindow / fitWindow /
//      regionWindow), including the 100ns minimum-duration clamp and the "clamp
//      each edge independently" behaviour near a bound. No store, no DOM.
//
//   2. A store-bound `ViewportActions` object that applies those results through
//      the store (never rendering directly - input dispatches store actions
//      only, the scheduler coalesces) and threads the zoom-history stack so `z`
//      undoes the last committed view.
//
// The history records every COMMITTED discrete view (each zoom/fit/region/arrow
// step, plus one entry per pan-drag at its end - the pointer machine calls
// recordCurrent() on mouseup). Continuous pan frames call applyPan(), which
// updates the viewport WITHOUT recording, so a drag is one undo step, not
// hundreds. undo() pops the previous committed view and applies it.

import { createZoomHistory, type ZoomHistory } from "../../lib/interact/zoom-history.js";
import type { ViewerStore } from "../../store/store.js";
import type { ViewportSlice } from "../../types/state.js";

/** The minimum view duration in ns. */
export const MIN_VIEW_NS = 100;

/** Arrow / WASD pan step as a fraction of the visible duration. */
export const PAN_FRACTION = 0.2;

/** The two horizontal-nav directions: -1 = left (earlier), +1 = right (later). */
export type PanDirection = -1 | 1;

/** The mutable subset a view change produces - the new visible window. */
export interface ViewWindow {
  viewStart: number;
  viewEnd: number;
}

/**
 * Zoom the window by `factor` about `centerFrac` of the current view (0 = left
 * edge, 0.5 = middle, 1 = right edge). The new duration is clamped to a 100ns
 * floor, then each edge is clamped to [minTs, maxTs] INDEPENDENTLY (so zooming
 * out near a bound can yield a shorter effective span).
 */
export function zoomWindow(vp: ViewportSlice, factor: number, centerFrac = 0.5): ViewWindow {
  const viewDur = vp.viewEnd - vp.viewStart;
  const center = vp.viewStart + viewDur * centerFrac;
  const newDur = Math.max(viewDur * factor, MIN_VIEW_NS);
  return {
    viewStart: Math.max(vp.minTs, center - newDur * centerFrac),
    viewEnd: Math.min(vp.maxTs, center + newDur * (1 - centerFrac)),
  };
}

/** Fit the window to the full navigable extent ("Fit All"). */
export function fitWindow(vp: ViewportSlice): ViewWindow {
  return { viewStart: vp.minTs, viewEnd: vp.maxTs };
}

/**
 * Pan by `PAN_FRACTION` of the visible duration in `dir`, preserving the
 * duration and clamping to [minTs, maxTs]. At a bound the window butts against
 * it and keeps its width.
 */
export function panWindow(vp: ViewportSlice, dir: PanDirection): ViewWindow {
  const viewDur = vp.viewEnd - vp.viewStart;
  const shift = viewDur * PAN_FRACTION;
  if (dir < 0) {
    let viewStart = Math.max(vp.minTs, vp.viewStart - shift);
    let viewEnd = viewStart + viewDur;
    if (viewEnd > vp.maxTs) {
      viewEnd = vp.maxTs;
      viewStart = viewEnd - viewDur;
    }
    return { viewStart, viewEnd };
  }
  let viewEnd = Math.min(vp.maxTs, vp.viewEnd + shift);
  let viewStart = viewEnd - viewDur;
  if (viewStart < vp.minTs) {
    viewStart = vp.minTs;
    viewEnd = viewStart + viewDur;
  }
  return { viewStart, viewEnd };
}

/**
 * Zoom the view to a selected [start, end] region (Alt+drag and the keyboard
 * zoom-selection confirm), clamped to [minTs, maxTs]. Direct edge clamp, no
 * 100ns floor - the caller guarantees a non-trivial region: >3px for drag,
 * >=100ns for keyboard.
 */
export function regionWindow(vp: ViewportSlice, start: number, end: number): ViewWindow {
  return {
    viewStart: Math.max(vp.minTs, Math.min(start, end)),
    viewEnd: Math.min(vp.maxTs, Math.max(start, end)),
  };
}

/** Two windows are the same view (history equality; avoids no-op undo steps). */
function sameWindow(a: ViewWindow, b: ViewWindow): boolean {
  return a.viewStart === b.viewStart && a.viewEnd === b.viewEnd;
}

/**
 * The store-bound viewport actions. Every method dispatches a store update and
 * NEVER renders; the store's RAF scheduler coalesces. Discrete view changes are
 * recorded in the zoom-history stack so `z` (undo) can step back.
 */
export interface ViewportActions {
  /** Zoom by factor about centerFrac (default view centre). */
  zoom(factor: number, centerFrac?: number): void;
  /** `f`: fit to the full trace extent. */
  fit(): void;
  /** Pan one step in `dir`, preserving duration. */
  pan(dir: PanDirection): void;
  /** Keyboard zoom-select confirm: zoom the view to [start, end]. */
  zoomToRegion(start: number, end: number): void;
  /** Drag frame: apply a panned window WITHOUT recording history. */
  applyPan(win: ViewWindow): void;
  /** Record the CURRENT viewport as a history entry (pan-drag end / baseline). */
  recordCurrent(): void;
  /** Reset history (new trace loaded); optionally seed a baseline. */
  resetHistory(): void;
  /**
   * `z` zoom-undo: pop the previous committed view and apply it. Returns true
   * when a view was restored, false when there is nothing to undo.
   */
  undo(): boolean;
  /** Available undo depth (for tests / status). */
  undoDepth(): number;
}

/** Build the store-bound viewport actions over `store`. */
export function createViewportActions(store: ViewerStore): ViewportActions {
  const history: ZoomHistory<ViewWindow> = createZoomHistory<ViewWindow>({ equals: sameWindow });

  function currentWindow(): ViewWindow {
    const vp = store.getState().viewport;
    return { viewStart: vp.viewStart, viewEnd: vp.viewEnd };
  }

  /** Apply a new window through the store, then record it (discrete commit). */
  function commit(win: ViewWindow): void {
    store.update("viewport", { viewStart: win.viewStart, viewEnd: win.viewEnd });
    history.record(win);
  }

  return {
    zoom(factor, centerFrac = 0.5): void {
      commit(zoomWindow(store.getState().viewport, factor, centerFrac));
    },
    fit(): void {
      commit(fitWindow(store.getState().viewport));
    },
    pan(dir): void {
      commit(panWindow(store.getState().viewport, dir));
    },
    zoomToRegion(start, end): void {
      commit(regionWindow(store.getState().viewport, start, end));
    },
    applyPan(win): void {
      store.update("viewport", { viewStart: win.viewStart, viewEnd: win.viewEnd });
    },
    recordCurrent(): void {
      history.record(currentWindow());
    },
    resetHistory(): void {
      history.clear();
    },
    undo(): boolean {
      const prev = history.undo();
      if (prev === null) return false;
      store.update("viewport", { viewStart: prev.viewStart, viewEnd: prev.viewEnd });
      return true;
    },
    undoDepth(): number {
      return history.depth();
    },
  };
}
