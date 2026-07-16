// The aggregated-mode (`?api=1`) poll-duration histogram minimap (#663): a
// collapsible strip under the toolbar showing the sample-weighted, log2-bucketed
// poll-duration distribution. Drag to select a poll-duration BAND (sets
// min_poll_ns/max_poll_ns and re-streams); click to move the fast/slow SPLIT
// (yellow line), then "Diff fast vs slow" opens a two-sided diff of the two
// halves. Ported from flamegraph.html's inline renderMinimap; the pure math is
// the frozen flamegraph_histogram.js core (via the lib/canvas seam).
//
// All the histogram/brush math is DOM-free and unit-tested in the frozen core;
// this module owns only the DOM + brush wiring. A render failure must never
// strand the flamegraph, so api-mode calls render() inside a try/catch.

import {
  brushToBand,
  fmtDurationNs,
  histogramLayout,
  normalizeHistogram,
  pxToNs,
  sampleWeightedMedianNs,
  type HistogramColumn,
  type PollHistogramBar,
} from "../../lib/canvas/index.js";
import { msToNs, nsToMs } from "../../lib/trace/index.js";
import type { PollDurationBar } from "../../lib/trace/index.js";

const MINIMAP_H = 46; // px, bar-area height
const SVG_NS = "http://www.w3.org/2000/svg";

export interface PollMinimapOptions {
  /** The minimap panel is inserted immediately after this element (the toolbar). */
  anchor: Element;
  /** Shared control style for the ms-band <input>s (matches the toolbar). */
  ctlStyle: string;
  /** Seed band bounds (ns strings) from the URL; null = no bound. */
  initialMinPollNs: string | null;
  initialMaxPollNs: string | null;
  /** Fired when the user commits a new band (Apply / Clear / brush release). */
  onApply: () => void;
  /**
   * Fired when the user opens a fast-vs-slow diff at the current split (ns).
   * Optional so the minimap works without the diff dispatch wired.
   */
  onDiffFastSlow?: (splitNs: number) => void;
}

export interface PollMinimap {
  /**
   * Render (or hide) the strip from a response's poll-duration histogram.
   * Safe to call every snapshot; skips the rebuild when nothing changed.
   */
  render(bars: PollDurationBar[] | undefined): void;
  /** The current band as ns strings (for the query state). */
  band(): { minPollNs: string | null; maxPollNs: string | null };
}

interface DragState {
  bars: PollHistogramBar[];
  width: number;
  brush: SVGRectElement;
  localX: (clientX: number) => number;
  start: number;
  moved: boolean;
}

/** Build the collapsible poll-duration minimap and return its controller. */
export function createPollMinimap(opts: PollMinimapOptions): PollMinimap {
  // ── ms band inputs (detached; live in the header, referenced by band()) ──
  const fPollMin = document.createElement("input");
  const fPollMax = document.createElement("input");
  for (const el of [fPollMin, fPollMax]) {
    el.type = "number";
    el.min = "0";
    el.step = "0.1";
    el.placeholder = "ms";
    el.style.cssText = opts.ctlStyle + ";width:70px";
  }
  fPollMin.value = nsToMs(opts.initialMinPollNs);
  fPollMax.value = nsToMs(opts.initialMaxPollNs);

  // ── Panel scaffold: display:none until a response carries a histogram ──
  const minimap = document.createElement("div");
  minimap.id = "f-minimap";
  minimap.style.cssText =
    "display:none;padding:6px 12px 8px;background:#141428;border-bottom:1px solid #333;flex-shrink:0;";

  const mmHeader = document.createElement("div");
  mmHeader.style.cssText =
    "display:flex;align-items:center;gap:10px;font-size:0.75em;color:#9a9ab0;flex-wrap:wrap";
  const mmToggle = document.createElement("button");
  mmToggle.style.cssText =
    "background:none;border:none;color:#c0c0d0;cursor:pointer;font-weight:600;font-size:1em;padding:0";
  const mmTitle = document.createElement("span");
  mmTitle.style.cssText = "font-weight:600;color:#c0c0d0";
  mmTitle.textContent = "Poll duration";
  const mmHint = document.createElement("span");
  mmHint.textContent = "samples per duration · drag to select a band · click to set split";
  const mmSpacer = document.createElement("span");
  mmSpacer.style.flex = "1";
  const geLabel = document.createElement("label");
  geLabel.style.color = "#e0e0e0";
  geLabel.append("≥ ", fPollMin, " ms");
  const leLabel = document.createElement("label");
  leLabel.style.color = "#e0e0e0";
  leLabel.append("≤ ", fPollMax, " ms");
  const mmApply = document.createElement("button");
  mmApply.textContent = "Apply band";
  mmApply.style.cssText =
    "background:#6c63ff;color:#fff;border:none;padding:2px 10px;border-radius:3px;cursor:pointer;font-weight:600";
  const mmFastSlow = document.createElement("button");
  mmFastSlow.style.cssText =
    "background:#2a2a4a;color:#e0e0e0;border:1px solid #444;padding:2px 10px;border-radius:3px;cursor:pointer;font-weight:600;display:none";
  const mmClear = document.createElement("button");
  mmClear.textContent = "Clear band";
  mmClear.style.cssText =
    "background:#2a2a4a;color:#e0e0e0;border:1px solid #444;padding:2px 10px;border-radius:3px;cursor:pointer";
  mmHeader.append(mmToggle, mmTitle, mmHint, mmSpacer, geLabel, leLabel, mmApply, mmFastSlow, mmClear);

  const mmBody = document.createElement("div");
  mmBody.id = "f-minimap-body";
  minimap.append(mmHeader, mmBody);
  opts.anchor.after(minimap);

  // ── State ──
  let minimapCollapsed = false;
  let splitNs: number | null = null;
  let renderedKey: string | null = null;
  let built = false;
  let dragging = false;
  let drag: DragState | null = null;
  let layout: { bars: PollHistogramBar[]; width: number; cols: HistogramColumn[] } | null = null;

  function setCollapsed(c: boolean): void {
    minimapCollapsed = c;
    mmBody.style.display = c ? "none" : "block";
    mmHint.style.display = c ? "none" : "";
    mmToggle.textContent = c ? "▸ Poll duration" : "▾";
    mmTitle.style.display = c ? "none" : "";
  }
  mmToggle.addEventListener("click", () => setCollapsed(!minimapCollapsed));
  setCollapsed(false);

  // Keep the "Diff fast vs slow (split @ …)" button label in sync with the
  // current split; hidden until a split exists.
  function updateFastSlowLabel(): void {
    if (splitNs == null || opts.onDiffFastSlow === undefined) {
      mmFastSlow.style.display = "none";
      return;
    }
    const s = fmtDurationNs(splitNs);
    mmFastSlow.style.display = "";
    mmFastSlow.textContent = `Diff fast vs slow polls (split @ ${s})`;
    mmFastSlow.title =
      `Open a two-panel flamegraph diff: fast polls (< ${s}) on the left, ` +
      `slow polls (≥ ${s}) on the right. Click the histogram to move the split.`;
  }

  function render(barsRaw: PollDurationBar[] | undefined): void {
    const bars = normalizeHistogram(barsRaw);
    if (bars.length === 0) {
      minimap.style.display = "none";
      return;
    }
    if (dragging) return; // don't clobber an in-progress brush
    minimap.style.display = "block";

    // Seed the split to the sample-weighted median the first time we see a
    // histogram for this scope; a user click overrides it.
    if (splitNs == null) splitNs = sampleWeightedMedianNs(bars);

    const key =
      bars.map((b) => `${b.lo_ns}:${b.samples}`).join(",") +
      `|${splitNs}|${fPollMin.value}|${fPollMax.value}`;
    if (key === renderedKey && built) {
      updateFastSlowLabel();
      return;
    }
    renderedKey = key;
    built = true;

    const width = Math.max(200, mmBody.clientWidth || minimap.clientWidth - 24 || 600);
    const { cols, maxSamples } = histogramLayout(bars, width, 2);
    layout = { bars, width, cols };

    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("width", "100%");
    svg.setAttribute("viewBox", `0 0 ${width} ${MINIMAP_H}`);
    svg.setAttribute("preserveAspectRatio", "none");
    svg.style.cssText = "display:block;cursor:crosshair;overflow:visible";

    // Current band -> shade the selected bars (continuous ns bounds, so a
    // sub-bucket band shades partially-overlapping bars too).
    const curMin = msToNs(fPollMin.value);
    const curMax = msToNs(fPollMax.value);
    const banded = curMin != null || curMax != null;
    for (const c of cols) {
      const inBand =
        (curMin == null || c.hi_ns > Number(curMin)) &&
        (curMax == null || c.lo_ns < Number(curMax));
      const rect = document.createElementNS(SVG_NS, "rect");
      const h = Math.max(1, c.hFrac * (MINIMAP_H - 2));
      rect.setAttribute("x", String(c.x));
      rect.setAttribute("y", String(MINIMAP_H - h));
      rect.setAttribute("width", String(c.w));
      rect.setAttribute("height", String(h));
      rect.setAttribute("fill", banded ? (inBand ? "#6c63ff" : "#3a3a5a") : "#6c63ff");
      svg.appendChild(rect);
    }

    // Split marker: a vertical line at splitNs, positioned by log-interp.
    const nsToPx = (ns: number): number => {
      for (let i = 0; i < cols.length; i++) {
        const c = cols[i]!;
        if (ns < c.hi_ns || i === cols.length - 1) {
          const frac = Math.log(Math.max(ns, c.lo_ns) / c.lo_ns) / Math.log(c.hi_ns / c.lo_ns);
          return c.x + Math.max(0, Math.min(1, frac)) * (width / cols.length);
        }
      }
      return 0;
    };
    if (splitNs != null) {
      const sx = nsToPx(splitNs);
      const line = document.createElementNS(SVG_NS, "line");
      line.setAttribute("x1", String(sx));
      line.setAttribute("x2", String(sx));
      line.setAttribute("y1", "0");
      line.setAttribute("y2", String(MINIMAP_H));
      line.setAttribute("stroke", "#ffcc55");
      line.setAttribute("stroke-width", "1.5");
      svg.appendChild(line);
    }

    // Brush overlay for drag-select.
    const brush = document.createElementNS(SVG_NS, "rect");
    brush.setAttribute("fill", "rgba(255,255,255,0.12)");
    brush.setAttribute("stroke", "#fff");
    brush.setAttribute("stroke-dasharray", "3 2");
    brush.setAttribute("y", "0");
    brush.setAttribute("height", String(MINIMAP_H));
    brush.style.display = "none";
    svg.appendChild(brush);

    const localX = (clientX: number): number => {
      const r = svg.getBoundingClientRect();
      return ((clientX - r.left) / r.width) * width;
    };
    svg.addEventListener("mousedown", (e) => {
      const start = localX(e.clientX);
      dragging = true;
      drag = { bars, width, brush, localX, start, moved: false };
      brush.setAttribute("x", String(start));
      brush.setAttribute("width", "0");
      brush.style.display = "block";
      e.preventDefault();
    });

    // X-axis ticks (in an HTML row so preserveAspectRatio="none" can't stretch
    // them). Thin to ~1 per 60px.
    const axis = document.createElement("div");
    axis.style.cssText =
      "position:relative;height:1.2em;margin-top:2px;font-size:0.68em;color:#8a8aa0";
    const nBars = cols.length;
    const stride = Math.max(1, Math.ceil(nBars / Math.max(1, Math.floor(width / 60))));
    for (let i = 0; i < nBars; i++) {
      if (i % stride !== 0) continue;
      const c = cols[i]!;
      const t = document.createElement("span");
      t.textContent = fmtDurationNs(c.lo_ns);
      t.style.cssText = `position:absolute;left:${(c.x / width) * 100}%;white-space:nowrap;transform:translateX(-1px)`;
      axis.appendChild(t);
    }
    if (nBars > 0) {
      const last = document.createElement("span");
      last.textContent = fmtDurationNs(cols[nBars - 1]!.hi_ns);
      last.style.cssText = "position:absolute;right:0;white-space:nowrap;color:#6a6a80";
      axis.appendChild(last);
    }

    const yhint = document.createElement("div");
    yhint.style.cssText = "font-size:0.66em;color:#6a6a80;margin-top:1px";
    yhint.textContent = `tallest bar = ${maxSamples.toLocaleString()} samples · yellow line = fast/slow split`;

    mmBody.replaceChildren(svg, axis, yhint);

    // Cursor-following tooltip (one reused body-level element).
    let tip = document.getElementById("f-minimap-tip");
    if (tip === null) {
      tip = document.createElement("div");
      tip.id = "f-minimap-tip";
      tip.style.cssText =
        "position:fixed;pointer-events:none;background:#000;color:#fff;border:1px solid #555;border-radius:3px;padding:3px 7px;font-size:0.72em;white-space:nowrap;z-index:1000;display:none";
      document.body.appendChild(tip);
    }
    const tipEl = tip;
    const barAtX = (px: number): HistogramColumn | undefined =>
      cols[Math.max(0, Math.min(cols.length - 1, Math.floor((px / width) * cols.length)))];
    svg.addEventListener("mousemove", (e) => {
      if (dragging) {
        tipEl.style.display = "none";
        return;
      }
      const c = barAtX(localX(e.clientX));
      if (c === undefined) {
        tipEl.style.display = "none";
        return;
      }
      tipEl.textContent =
        `${fmtDurationNs(c.lo_ns)}–${fmtDurationNs(c.hi_ns)} · ${c.samples.toLocaleString()} samples`;
      tipEl.style.display = "block";
      tipEl.style.left = `${e.clientX + 12}px`;
      tipEl.style.top = `${e.clientY - 28}px`;
    });
    svg.addEventListener("mouseleave", () => {
      tipEl.style.display = "none";
    });

    updateFastSlowLabel();
  }

  // ── Band commit / clear ──
  function applyBand(): void {
    renderedKey = null; // force a rebuild so the new shading appears at once
    if (layout) render(layout.bars);
    opts.onApply();
  }
  mmApply.addEventListener("click", applyBand);
  for (const el of [fPollMin, fPollMax]) {
    el.addEventListener("keydown", (e) => {
      if (e.key === "Enter") applyBand();
    });
  }
  mmClear.addEventListener("click", () => {
    fPollMin.value = "";
    fPollMax.value = "";
    applyBand();
  });
  mmFastSlow.addEventListener("click", () => {
    if (splitNs != null) opts.onDiffFastSlow?.(splitNs);
  });

  // ── Window-level brush move/up (attached ONCE; render must not add these
  // per rebuild). Act only while a drag is armed. ──
  window.addEventListener("mousemove", (e) => {
    if (!dragging || drag === null) return;
    const x = drag.localX(e.clientX);
    if (Math.abs(x - drag.start) > 2) drag.moved = true;
    drag.brush.setAttribute("x", String(Math.min(drag.start, x)));
    drag.brush.setAttribute("width", String(Math.abs(x - drag.start)));
  });
  window.addEventListener("mouseup", (e) => {
    if (!dragging || drag === null) return;
    const d = drag;
    dragging = false;
    drag = null;
    d.brush.style.display = "none";
    const endX = d.localX(e.clientX);
    const bandSel = brushToBand(d.bars, d.width, d.start, endX);
    if (bandSel) {
      // A real drag -> set the exact (continuous) ns band and re-stream.
      fPollMin.value = nsToMs(String(bandSel.min_ns));
      fPollMax.value = nsToMs(String(bandSel.max_ns));
      applyBand();
    } else if (!d.moved) {
      // A click (no drag) -> move the fast/slow split here. No re-stream.
      const ns = pxToNs(d.bars, d.width, endX);
      if (ns != null) {
        splitNs = ns;
        renderedKey = null;
        render(d.bars);
      }
    }
  });

  return {
    render,
    band() {
      return { minPollNs: msToNs(fPollMin.value), maxPollNs: msToNs(fPollMax.value) };
    },
  };
}
