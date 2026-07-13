// components/canvas/lanes/legend.ts - the worker-lanes legend (features/02
// G19), with the 04 F3 amendment: the legend must cover EVERY in-lane mark,
// including the "q:NN" local-queue label and the marks the legacy toolbar
// legend omitted (block_in_place, open-ended poll, wake, sched, CPU sample).
//
// Legacy G19 was a non-interactive toolbar row (viewer.html:6646-6670). The
// unified column has no legacy toolbar; the legend is a compact, non-
// interactive (pointer-events:none) ribbon overlaid on the lanes track's
// canvas wrap. It is built imperatively into the statically-templated wrap
// (the wrap holds only a static <canvas>, no lit-html child binding), so the
// shell's declarative re-renders do not clobber it (same technique the toast
// region uses).

const LEGEND_CLASS = "d9-lanes-legend";

/** One legend row: a color/marker swatch and its meaning. */
export interface LegendEntry {
  /** Swatch fill (CSS color) or a gradient hint string. */
  swatch: string;
  label: string;
  /** Rendering hint so the swatch can draw a line/triangle/hatch, not a box. */
  shape?: "box" | "gradient" | "line" | "dashed" | "triangle" | "tick" | "hatch";
}

/**
 * The legend contract (F3): every visual the lanes draw must be explained.
 * Ordered to match a reader scanning a lane top -> bottom.
 */
export const LANES_LEGEND: readonly LegendEntry[] = [
  { swatch: "linear-gradient(90deg,#0a1a3a,#00bcd4,#ff9800,#f44336)", label: "poll (short -> long)", shape: "gradient" },
  { swatch: "#ffeb3b", label: "selected task / span", shape: "box" },
  { swatch: "#ff8a65", label: "waker task", shape: "box" },
  { swatch: "#2a1520", label: "parked", shape: "box" },
  { swatch: "#ff0000", label: "kernel sched delay", shape: "box" },
  { swatch: "#3a2a00", label: "block_in_place", shape: "hatch" },
  { swatch: "#ff9800", label: "open-ended poll", shape: "dashed" },
  { swatch: "#ce93d8", label: "CPU sample", shape: "tick" },
  { swatch: "#ff2222", label: "sched (blocking)", shape: "triangle" },
  { swatch: "#66bb6a", label: "wake", shape: "triangle" },
  { swatch: "rgba(255,200,50,0.8)", label: "local queue (q:NN)", shape: "line" },
];

/** The lanes track's canvas wrap (statically templated by the shell). */
function laneCanvasWrap(trackColumn: HTMLElement): HTMLElement | null {
  return trackColumn.querySelector<HTMLElement>(
    `.d9-track[data-track-id="lanes"] .d9-track-canvas-wrap`,
  );
}

/**
 * Ensure the legend ribbon exists in the lanes canvas wrap (idempotent):
 * appends it if the wrap is present and the legend is missing. Safe to call
 * every frame - it is a no-op once the legend is attached.
 */
export function ensureLanesLegend(trackColumn: HTMLElement): HTMLElement | null {
  const wrap = laneCanvasWrap(trackColumn);
  if (!wrap) return null;
  const existing = wrap.querySelector<HTMLElement>(`.${LEGEND_CLASS}`);
  if (existing) return existing;

  const list = document.createElement("ul");
  list.className = LEGEND_CLASS;
  list.setAttribute("aria-label", "Worker lane legend");
  for (const entry of LANES_LEGEND) {
    const li = document.createElement("li");
    li.className = "d9-lanes-legend-row";
    const swatch = document.createElement("span");
    swatch.className = `d9-lanes-legend-swatch shape-${entry.shape ?? "box"}`;
    swatch.style.background = entry.swatch;
    // Shapes drawn with borders (triangle/dashed) read the entry color via
    // currentColor rather than a fill.
    swatch.style.color = entry.swatch;
    const text = document.createElement("span");
    text.className = "d9-lanes-legend-text";
    text.textContent = entry.label;
    li.appendChild(swatch);
    li.appendChild(text);
    list.appendChild(li);
  }
  wrap.appendChild(list);
  return list;
}

/** Remove the legend (teardown / HMR). */
export function removeLanesLegend(trackColumn: HTMLElement): void {
  const wrap = laneCanvasWrap(trackColumn);
  wrap?.querySelector(`.${LEGEND_CLASS}`)?.remove();
}

/** Mount the legend and return a disposer (removes it). */
export function mountLanesLegend(trackColumn: HTMLElement): () => void {
  ensureLanesLegend(trackColumn);
  return () => removeLanesLegend(trackColumn);
}
