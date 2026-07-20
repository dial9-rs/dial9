// The transient crosshair overlay: mouse crosshair, keyboard cursor,
// custom-event guide, pinned-event marker.
//
// The overlay is a SEPARATE canvas on its OWN RAF channel: it subscribes to the
// `transient` slice (+ viewport/selection/trace for line placement) and redraws
// ONLY this canvas, never the full track column (a mousemove updates transient,
// which notifies only transient subscribers; the lanes subscribe to
// trace/viewport/selection, NOT transient, so a hover never redraws them). The
// backing store resizes ONLY on geometry change (lib/canvas/dpr).
//
// ALIGNMENT INVARIANT: the crosshair uses the SHARED layout math -
// `time.nsToPanelX(ns)` from lib/canvas/layout, the exact mapping the
// lanes/axis use - so a line at `ns` lands pixel-exact on the poll/tick at `ns`
// below it. The overlay canvas spans the full column width (x=0 at the
// label-gutter left edge), and nsToPanelX is already LABEL_W-shifted, so
// `crosshairX(ns) === nsToPanelX(ns)` equals the lanes' absolute-in-column x
// (LABEL_W + their canvas-local `nsToPanelX(ns) - labelW`) by construction.
// Never compute a private x-mapping here.

import type { KeyboardSelection, TimePanelLayout } from "../../types/state.js";

/** The minimal 2D-context surface `drawCrosshair` needs (Node-testable). */
export interface CrosshairContext {
  strokeStyle: string | CanvasGradient | CanvasPattern;
  fillStyle: string | CanvasGradient | CanvasPattern;
  lineWidth: number;
  font: string;
  textAlign: CanvasTextAlign;
  textBaseline: CanvasTextBaseline;
  clearRect(x: number, y: number, w: number, h: number): void;
  beginPath(): void;
  moveTo(x: number, y: number): void;
  lineTo(x: number, y: number): void;
  stroke(): void;
  fillRect(x: number, y: number, w: number, h: number): void;
  strokeRect(x: number, y: number, w: number, h: number): void;
  fillText(text: string, x: number, y: number): void;
  measureText(text: string): { width: number };
  setLineDash(segments: number[]): void;
}

/** The pinned custom-event marker: timestamp + a preformatted chip label. */
export interface CrosshairPinnedEvent {
  timestamp: number;
  /** "name @ HH:MM:SS" style chip label, formatted by the caller (clock mode). */
  label: string;
}

/** Everything one overlay draw pass reads; pure over these inputs. */
export interface CrosshairInput {
  /** Shared ns<->x mapping (LABEL_W-shifted; the alignment invariant source). */
  time: TimePanelLayout;
  /** Overlay canvas CSS width (full column width, pw). */
  width: number;
  /** Overlay canvas CSS height (total stacked track height). */
  height: number;
  viewStart: number;
  viewEnd: number;
  /** Mouse timestamp; null hides the dashed crosshair. */
  mouseNs: number | null;
  /** Keyboard-selection cursor; null when not selecting. */
  keyboardSelection: KeyboardSelection | null;
  /** Hovered custom-event timestamp for the orange guide; null hides it. */
  hoverEventTs: number | null;
  /** Pinned custom-event marker + label chip; null when none pinned. */
  pinnedEvent: CrosshairPinnedEvent | null;
}

/**
 * The crosshair x (overlay-space CSS px) for a timestamp: the SHARED layout
 * mapping, unchanged. Equals the lanes' absolute-in-column x for the same ns
 * (the alignment invariant) - the whole point of not inventing a private
 * mapping here.
 */
export function crosshairX(time: TimePanelLayout, ns: number): number {
  return time.nsToPanelX(ns);
}

/** True when `ns` is within the visible window (inclusive). */
function inView(ns: number, viewStart: number, viewEnd: number): boolean {
  return ns >= viewStart && ns <= viewEnd;
}

/**
 * Draw all crosshair overlays into `ctx`. The context transform is assumed
 * already DPR-scaled by the caller (lib/canvas/dpr sizer), so coordinates are
 * CSS px. Pure over its inputs; the only mutable state is the canvas. Zero DOM
 * reads (the overlay path never measures layout).
 */
export function drawCrosshair(ctx: CrosshairContext, input: CrosshairInput): void {
  const { width, height, viewStart, viewEnd } = input;
  ctx.clearRect(0, 0, width, height);
  if (width <= 0 || height <= 0 || viewEnd <= viewStart) return;

  // Mouse crosshair: dashed white vertical line.
  if (input.mouseNs !== null && inView(input.mouseNs, viewStart, viewEnd)) {
    const x = crosshairX(input.time, input.mouseNs);
    ctx.strokeStyle = "rgba(255,255,255,0.3)";
    ctx.lineWidth = 1;
    ctx.setLineDash([4, 4]);
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, height);
    ctx.stroke();
    ctx.setLineDash([]);
  }

  // Keyboard-selection cursor: solid bright line at the moving cursor.
  const kb = input.keyboardSelection;
  if (kb !== null && inView(kb.cursorNs, viewStart, viewEnd)) {
    const x = crosshairX(input.time, kb.cursorNs);
    ctx.strokeStyle = "rgba(255,255,255,0.8)";
    ctx.lineWidth = 1.5;
    ctx.setLineDash([]);
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, height);
    ctx.stroke();
  }

  // Custom-event hover guide: orange dashed line across all lanes.
  if (input.hoverEventTs !== null && inView(input.hoverEventTs, viewStart, viewEnd)) {
    const x = crosshairX(input.time, input.hoverEventTs);
    ctx.strokeStyle = "rgba(255,140,0,0.4)";
    ctx.lineWidth = 1;
    ctx.setLineDash([3, 3]);
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, height);
    ctx.stroke();
    ctx.setLineDash([]);
  }

  // Pinned custom-event marker: persistent orange dashed line + label chip.
  const pin = input.pinnedEvent;
  if (pin !== null && inView(pin.timestamp, viewStart, viewEnd)) {
    const mx = crosshairX(input.time, pin.timestamp);
    ctx.strokeStyle = "rgba(255,140,0,0.9)";
    ctx.lineWidth = 1;
    ctx.setLineDash([3, 3]);
    ctx.beginPath();
    ctx.moveTo(mx, 0);
    ctx.lineTo(mx, height);
    ctx.stroke();
    ctx.setLineDash([]);

    // Label chip "name @ time", kept inside the overlay (flips left on overflow).
    const label = pin.label;
    ctx.font = "11px sans-serif";
    const tw = ctx.measureText(label).width;
    const padX = 5;
    let bx = mx + 4;
    if (bx + tw + padX * 2 > width) bx = mx - 4 - (tw + padX * 2);
    ctx.fillStyle = "rgba(40,25,0,0.92)";
    ctx.fillRect(bx, 2, tw + padX * 2, 16);
    ctx.strokeStyle = "rgba(255,140,0,0.9)";
    ctx.lineWidth = 1;
    ctx.strokeRect(bx, 2, tw + padX * 2, 16);
    ctx.fillStyle = "#ffcf99";
    ctx.textAlign = "left";
    ctx.textBaseline = "middle";
    ctx.fillText(label, bx + padX, 11);
  }
}
