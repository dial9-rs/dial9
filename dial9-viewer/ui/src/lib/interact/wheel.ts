// lib/interact/wheel.ts - the wheel semantics for the lane area (T23;
// features/02 H5, docs/ui-inventory/02-architecture.md 2.5).
//
// Plain wheel = let the browser scroll the worker lanes vertically (the DOM
// binding does nothing, no preventDefault). Ctrl/Cmd + wheel = zoom the
// timeline centred on the cursor. A pure function so the exact legacy mapping
// (viewer.html:5100-5116) is testable without a DOM: it returns the zoom
// intent (factor + centre fraction) or null for a plain wheel. The binding
// preventDefaults and applies the intent through the store (coalesced to <= 1
// render/frame by the RAF scheduler - the F2 regression the DoD guards).

/** The cursor-anchored zoom factor per wheel notch (legacy 1.3, viewer.html:5110). */
export const WHEEL_ZOOM_FACTOR = 1.3;

export interface WheelInput {
  /** Ctrl held (zoom on every platform). */
  ctrlKey: boolean;
  /** Cmd held (zoom on macOS). */
  metaKey: boolean;
  /** Wheel delta; > 0 = wheel down = zoom OUT (legacy `deltaY > 0`). */
  deltaY: number;
  /** Pointer x within the DRAW area (clientX - panel left - LABEL_W), CSS px. */
  mouseXInDraw: number;
  /** Draw-area width (CSS px). */
  drawW: number;
}

/** A cursor-anchored zoom request: `zoom(factor, centerFrac)`. */
export interface WheelZoomIntent {
  factor: number;
  centerFrac: number;
}

/**
 * Resolve a wheel event to a zoom intent, or null when it is a plain wheel
 * (scroll the lanes; the binding leaves the browser default alone). Mirrors
 * viewer.html:5100-5115: zoom only with Ctrl/Cmd, factor 1.3 out / 1/1.3 in,
 * centred on the cursor fraction clamped to [0,1].
 */
export function wheelZoomIntent(input: WheelInput): WheelZoomIntent | null {
  if (!input.ctrlKey && !input.metaKey) return null;
  const frac = input.drawW > 0 ? Math.max(0, Math.min(1, input.mouseXInDraw / input.drawW)) : 0.5;
  const factor = input.deltaY > 0 ? WHEEL_ZOOM_FACTOR : 1 / WHEEL_ZOOM_FACTOR;
  return { factor, centerFrac: frac };
}
