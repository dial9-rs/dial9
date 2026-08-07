// The duration histogram: log-spaced bars, P50/P90/P99/P99.9 guide lines, and
// a drag-to-select duration band.
//
// SVG is built imperatively rather than through lit-html: the brush needs
// direct node handles and pointer capture, and no interpolated value here comes
// from user data (durations and counts are numbers).

import {
  durationAtPercentile,
  fmtNs,
  normalizeSpanHistogram,
  spanBrushToBand,
  spanHistogramLayout,
  spanNsToPx,
} from "../../lib/trace/index.js";
import type { DurationBand, HistogramBarLike } from "../../lib/trace/index.js";
import { fmtDurationNs } from "../../lib/canvas/index.js";

const SVG_NS = "http://www.w3.org/2000/svg";
/** Drawing width in viewBox units; the SVG scales to its container. */
const W = 600;
const HIST_H = 50;

const PERCENTILES = [
  { p: 50, label: "P50" },
  { p: 90, label: "P90" },
  { p: 99, label: "P99" },
  { p: 99.9, label: "P99.9" },
] as const;

/** A native hover tooltip for an SVG node. */
function titleEl(text: string): SVGTitleElement {
  const t = document.createElementNS(SVG_NS, "title");
  t.textContent = text;
  return t;
}

/**
 * Draw the histogram into `container` and wire the brush.
 *
 * `onBand` fires once per completed drag; near-zero drags are treated as clicks
 * and produce nothing.
 */
export function renderHistogram(
  container: HTMLElement,
  histogram: readonly HistogramBarLike[] | undefined,
  band: DurationBand,
  onBand: (band: DurationBand) => void,
): void {
  // The container is a STATIC node in the panel's lit-html template, so
  // lit reuses it across renders and never clears it for us. Without this the
  // SVG, axis and hint stack up one full set per snapshot.
  container.replaceChildren();
  const bars = normalizeSpanHistogram(histogram);
  if (bars.length === 0) {
    const empty = document.createElement("div");
    empty.style.cssText = "color:#666;font-size:0.8em;padding:4px 0";
    empty.textContent = "No histogram data.";
    container.appendChild(empty);
    return;
  }

  const { cols, maxCount } = spanHistogramLayout(bars, W, 2);
  const banded = band.min_ns != null || band.max_ns != null;

  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("width", "100%");
  svg.setAttribute("viewBox", `0 0 ${W} ${HIST_H}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.style.cssText = "display:block;cursor:crosshair;overflow:visible;max-width:800px";

  for (const c of cols) {
    const inBand =
      (band.min_ns == null || c.hi_ns > band.min_ns) &&
      (band.max_ns == null || c.lo_ns < band.max_ns);
    const h = Math.max(1, c.hFrac * (HIST_H - 2));
    const rect = document.createElementNS(SVG_NS, "rect");
    rect.setAttribute("x", String(c.x));
    rect.setAttribute("y", String(HIST_H - h));
    rect.setAttribute("width", String(c.w));
    rect.setAttribute("height", String(h));
    // Out-of-band bars dim rather than disappear, so the shape of the whole
    // distribution stays legible while a band is selected.
    rect.setAttribute("fill", banded && !inBand ? "#3a3a5a" : "#6c63ff");
    svg.appendChild(rect);
  }

  // Percentile guides, positioned off the histogram's own log axis so they line
  // up with the columns rather than floating on a separate scale.
  for (const { p, label } of PERCENTILES) {
    const ns = durationAtPercentile(bars, p);
    if (ns == null) continue;
    const x = spanNsToPx(bars, W, ns);
    if (x == null) continue;
    const tip = `${label}: ${fmtNs(ns)}`;

    const line = document.createElementNS(SVG_NS, "line");
    line.setAttribute("x1", String(x));
    line.setAttribute("x2", String(x));
    line.setAttribute("y1", "0");
    line.setAttribute("y2", String(HIST_H));
    line.setAttribute("class", "pctile-line");
    line.appendChild(titleEl(tip));
    svg.appendChild(line);

    // Flip the label to the left near the right edge so it stays on screen.
    const nearRight = x > W - 40;
    const text = document.createElementNS(SVG_NS, "text");
    text.setAttribute("x", String(nearRight ? x - 3 : x + 3));
    text.setAttribute("y", "9");
    text.setAttribute("text-anchor", nearRight ? "end" : "start");
    text.setAttribute("class", "pctile-label");
    text.textContent = label;
    text.appendChild(titleEl(tip));
    svg.appendChild(text);
  }

  const brush = document.createElementNS(SVG_NS, "rect");
  brush.setAttribute("fill", "rgba(255,255,255,0.12)");
  brush.setAttribute("stroke", "#fff");
  brush.setAttribute("stroke-dasharray", "3 2");
  brush.setAttribute("y", "0");
  brush.setAttribute("height", String(HIST_H));
  brush.style.display = "none";
  svg.appendChild(brush);

  let dragStart: number | null = null;
  const localX = (clientX: number): number => {
    const r = svg.getBoundingClientRect();
    return ((clientX - r.left) / r.width) * W;
  };
  const endDrag = (): void => {
    dragStart = null;
    brush.style.display = "none";
  };

  // Pointer capture rather than window listeners: the panel re-renders on every
  // snapshot, and window listeners would accumulate one set per render.
  svg.addEventListener("pointerdown", (e: PointerEvent) => {
    dragStart = localX(e.clientX);
    brush.setAttribute("x", String(dragStart));
    brush.setAttribute("width", "0");
    brush.style.display = "block";
    svg.setPointerCapture(e.pointerId);
    e.preventDefault();
  });

  svg.addEventListener("pointermove", (e: PointerEvent) => {
    if (dragStart == null) return;
    const x = localX(e.clientX);
    brush.setAttribute("x", String(Math.min(dragStart, x)));
    brush.setAttribute("width", String(Math.abs(x - dragStart)));
  });

  svg.addEventListener("pointerup", (e: PointerEvent) => {
    if (dragStart == null) return;
    const from = dragStart;
    endDrag();
    svg.releasePointerCapture(e.pointerId);
    const next = spanBrushToBand(bars, W, from, localX(e.clientX));
    if (next) onBand(next);
  });

  // Capture can be lost when the element is removed mid-drag; reset rather than
  // leaving a stuck brush behind.
  svg.addEventListener("lostpointercapture", endDrag);

  container.appendChild(svg);

  const axis = document.createElement("div");
  axis.className = "histogram-axis";
  axis.style.maxWidth = "800px";
  const stride = Math.max(1, Math.ceil(cols.length / Math.max(1, Math.floor(W / 70))));
  cols.forEach((c, i) => {
    if (i % stride !== 0) return;
    const t = document.createElement("span");
    t.textContent = fmtDurationNs(c.lo_ns);
    t.style.cssText = `position:absolute;left:${(c.x / W) * 100}%;white-space:nowrap;transform:translateX(-1px)`;
    axis.appendChild(t);
  });
  container.appendChild(axis);

  const hint = document.createElement("div");
  hint.className = "histogram-hint";
  hint.textContent =
    `tallest bar = ${maxCount.toLocaleString()} instances · ` +
    "dashed lines mark P50/P90/P99/P99.9 · drag to select a duration band";
  container.appendChild(hint);
}
