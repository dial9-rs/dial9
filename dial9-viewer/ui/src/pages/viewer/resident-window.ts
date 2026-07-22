// The resident-window descriptor every windowed track surfaces, plus its
// derivation and its canvas markers.
//
// Segment windowing can leave a track's data covering less than the whole
// trace. The obligation that follows is shared, not per-track: a truncated or
// terminally-oversized window must never be PAINTED as complete. The CPU,
// queue and task-detail tracks each carried their own copy of this descriptor,
// its "complete" constant, its segment scan and its marker drawing; they now
// share one.

import type { StoreState } from "../../types/state.js";

/** Colors for the truncation band and the "partial window" label. */
const TRUNC_BAND = "rgba(255,120,120,0.28)";
const TRUNC_LABEL = "#ffb3b3";

/** Width of the edge band marking a truncated side, px. */
const BAND = 6;

/**
 * The resident-window state a windowed renderer must surface. Both fields
 * resolve to the "complete" value for a whole-trace load.
 */
export interface ResidentWindow {
  /**
   * Which edge(s) of the resident window truncate the data: data beyond the
   * edge is not resident, so that edge is a WINDOW boundary, not the true data
   * boundary. null when the window covers the whole trace.
   */
  truncatedAt: "start" | "end" | "both" | null;
  /**
   * True when a segment needed for this view is in the terminal "oversized"
   * state (SegmentLifecycle): it can never be resident, so the view is
   * unavoidably partial and must say so.
   */
  oversized: boolean;
}

/** The "complete window" descriptor (whole-trace load, no windowing). */
export const COMPLETE_WINDOW: ResidentWindow = {
  truncatedAt: null,
  oversized: false,
};

/** Derive the resident window from a store snapshot: any terminally-oversized
 * segment makes the whole view unavoidably partial. */
export function deriveResidentWindow(state: StoreState): ResidentWindow {
  for (const entry of state.segments.segments.values()) {
    if (entry.state === "oversized") {
      return { truncatedAt: null, oversized: true };
    }
  }
  return COMPLETE_WINDOW;
}

/**
 * Paint the truncation bands and the "partial window" label. A complete window
 * draws nothing, which falls out of the field tests -- deliberately NOT a
 * `window === COMPLETE_WINDOW` identity check, which silently stops working
 * for any equal-but-distinct descriptor.
 */
export function drawWindowMarkers(
  ctx: CanvasRenderingContext2D,
  window: ResidentWindow,
  drawW: number,
  height: number,
): void {
  const truncatedAt = window.truncatedAt;
  if (truncatedAt === "start" || truncatedAt === "both") {
    ctx.fillStyle = TRUNC_BAND;
    ctx.fillRect(0, 0, BAND, height);
  }
  if (truncatedAt === "end" || truncatedAt === "both") {
    ctx.fillStyle = TRUNC_BAND;
    ctx.fillRect(Math.max(0, drawW - BAND), 0, BAND, height);
  }
  if (window.oversized) {
    ctx.fillStyle = TRUNC_LABEL;
    ctx.font = "10px monospace";
    ctx.textAlign = "left";
    ctx.fillText("partial window", 3, height - 3);
  }
}
