// Browse view component: status/warning chrome, host-label column, the
// density heatmap canvas, the time axis and the reset-zoom button. Density
// math comes from heatmap.js (accumulateDensity, tiling, gaps, boot
// transitions, densityColor ramp).
//
// The paint is memoized on its redraw triggers: a new result set (rows
// identity), a domain change (zoom/reset), a TZ flip, and the debounced
// resize (browse.renderEpoch). Anything else that changes the browse slice
// (e.g. the selection) re-runs the cheap chrome writes only.

// Leaf heatmap seam, not the lib/canvas barrel (see actions.ts: the barrel
// evaluates trace_analysis.js, which the browser page must not carry).
import {
  accumulateDensity,
  bootTransitions,
  densityColor,
} from "../../lib/canvas/heatmap.js";
import { assertInScheduledRender } from "../../store/store.js";
import { ROW_H } from "./actions.js";
import type { PageCtx } from "./ctx.js";
import { clamp, crossesDayBoundary, fmtTick, timeToX } from "./format.js";
import type { HeatmapRow, TimeDomain } from "./state.js";
import { renderStatus } from "./status-render.js";

// Read the label-column width from the --heatmap-label-w CSS custom
// property so the JS axis layout and the CSS column width share a single
// source. Lazy so the stylesheet is applied first.
let labelW: number | null = null;
function LABEL_W(): number {
  if (labelW === null) {
    labelW =
      parseFloat(
        getComputedStyle(document.documentElement).getPropertyValue("--heatmap-label-w"),
      ) || 220;
  }
  return labelW;
}

// Precomputed 256-entry density color ramp. Building a color string per
// pixel column is the hot path; quantizing to 256 buckets lets us reuse
// strings (and, with run-length coalescing, collapse adjacent equal-color
// columns into a single fillRect).
let densityPalette: string[] | null = null;
function densityPaletteColor(norm: number): string {
  if (!densityPalette) {
    densityPalette = new Array<string>(256);
    for (let i = 0; i < 256; i++) densityPalette[i] = densityColor(i / 255);
  }
  const idx = norm >= 1 ? 255 : norm <= 0 ? 0 : (norm * 255) | 0;
  return densityPalette[idx]!;
}

export function mountBrowseView({ store, els }: PageCtx): void {
  let lastRows: readonly HeatmapRow[] | null = null;
  let lastDomain: TimeDomain | null = null;
  let lastTz: boolean | null = null;
  let lastEpoch = -1;

  // Host labels (one row per service/host; boot count annotated).
  // Unknown-layout groups render their raw directory path instead of a
  // guessed "service / host" split.
  function rebuildLabels(rows: readonly HeatmapRow[]): void {
    els.heatmapLabels.textContent = "";
    for (const row of rows) {
      const boots = bootTransitions(row.segments);
      const div = document.createElement("div");
      div.className = "row";
      if (row.segments[0]?.layout === "unknown") {
        // Raw display: `host` carries the group's raw directory path
        // (segments.ts unknownGroupPath); no service/host split exists.
        div.append(document.createTextNode(row.host));
      } else {
        const svc = document.createElement("span");
        svc.className = "svc";
        svc.textContent = row.service;
        div.append(svc, document.createTextNode(` / ${row.host}`));
      }
      if (boots.length) {
        div.append(document.createTextNode(" "));
        const boot = document.createElement("span");
        boot.className = "boot";
        boot.title = `boot id changes ${boots.length} time(s) in this window`;
        boot.textContent = `▏${boots.length + 1} boots`;
        div.append(boot);
      }
      // Unknown-layout rows: the groupByHost label is " / <path>"; the raw
      // path alone is the honest tooltip.
      div.title = row.segments[0]?.layout === "unknown" ? row.host : row.label;
      els.heatmapLabels.appendChild(div);
    }
  }

  function drawCanvas(rows: readonly HeatmapRow[], domain: TimeDomain, tz: boolean): void {
    const W = Math.max(50, Math.floor(els.heatmapPlot.clientWidth));
    const H = rows.length * ROW_H;
    const dpr = window.devicePixelRatio || 1;
    const canvas = els.heatmapCanvas;
    canvas.width = Math.floor(W * dpr);
    canvas.height = Math.floor(H * dpr);
    canvas.style.width = W + "px";
    canvas.style.height = H + "px";
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, W, H);

    const { tMin, tMax } = domain;

    // Pass 1: per-row density columns + global max (for normalization).
    // Uses tiled segments (ends clamped to the next start) so upload-lag
    // overlaps at segment seams don't double-count into a bright artifact.
    const cols = rows.map((r) => accumulateDensity(r.tiled, tMin, tMax, W));
    let colMax = 0;
    for (const c of cols) {
      for (let i = 0; i < c.length; i++) {
        if (c[i]! > colMax) colMax = c[i]!;
      }
    }

    // Pass 2: draw each row's strip, then boot-change markers on top.
    for (let r = 0; r < rows.length; r++) {
      const rowCols = cols[r]!;
      const y = r * ROW_H;
      // Alternating base background so empty rows stay visually distinct.
      ctx.fillStyle = r % 2 ? "#20203e" : "#191932";
      ctx.fillRect(0, y, W, ROW_H);
      // Paint density as coalesced runs of identical color (precomputed
      // palette + run-length merge; empty columns fall through to the base
      // background above).
      let runStart = -1;
      let runColor: string | null = null;
      for (let x = 0; x < W; x++) {
        const norm = colMax > 0 ? rowCols[x]! / colMax : 0;
        const color = norm > 0 ? densityPaletteColor(norm) : null;
        if (color !== runColor) {
          if (runColor !== null) {
            ctx.fillStyle = runColor;
            ctx.fillRect(runStart, y, x - runStart, ROW_H);
          }
          runColor = color;
          runStart = x;
        }
      }
      if (runColor !== null) {
        ctx.fillStyle = runColor;
        ctx.fillRect(runStart, y, W - runStart, ROW_H);
      }
      // Coverage gaps: a host that stopped reporting leaves a real hole
      // between segments (vs. the seamless rotation overlaps the tiling
      // removed). Mark each with a faint hatched band plus thin edge ticks
      // so it reads as "no data here", distinct from low-but-present
      // density. Sub-pixel gaps still get a 1px tick so they're not lost.
      for (const g of rows[r]!.gaps) {
        const gx0 = timeToX(Math.max(g.start, tMin), tMin, tMax, W);
        const gx1 = timeToX(Math.min(g.end, tMax), tMin, tMax, W);
        if (gx1 < 0 || gx0 > W) continue;
        const lo = Math.max(0, gx0);
        const hi = Math.min(W, gx1);
        const gw = Math.max(1, hi - lo);
        ctx.save();
        ctx.beginPath();
        ctx.rect(lo, y + 1, gw, ROW_H - 2);
        ctx.clip();
        ctx.fillStyle = "#15152a";
        ctx.fillRect(lo, y + 1, gw, ROW_H - 2);
        // Diagonal hatch.
        ctx.strokeStyle = "rgba(120,120,160,0.35)";
        ctx.lineWidth = 1;
        ctx.beginPath();
        for (let hx = lo - ROW_H; hx < hi + ROW_H; hx += 6) {
          ctx.moveTo(hx, y + ROW_H);
          ctx.lineTo(hx + ROW_H, y);
        }
        ctx.stroke();
        ctx.restore();
        // Edge ticks frame the hole so even a 1px gap stays visible.
        ctx.fillStyle = "rgba(150,150,190,0.6)";
        ctx.fillRect(Math.round(lo), y + 1, 1, ROW_H - 2);
        if (hi - lo > 2) ctx.fillRect(Math.round(hi) - 1, y + 1, 1, ROW_H - 2);
      }
      // Boot-change dividers: a consistent dashed cyan line so they read
      // as boundaries, never as a data spike.
      ctx.save();
      ctx.strokeStyle = "#00e5ff";
      ctx.lineWidth = 1;
      ctx.setLineDash([4, 3]);
      for (const b of bootTransitions(rows[r]!.segments)) {
        const bx = Math.round(timeToX(b.time, tMin, tMax, W)) + 0.5;
        ctx.beginPath();
        ctx.moveTo(bx, y + 1);
        ctx.lineTo(bx, y + ROW_H - 1);
        ctx.stroke();
      }
      ctx.restore();
      // Row separator.
      ctx.fillStyle = "#3a3a5e";
      ctx.fillRect(0, y + ROW_H - 1, W, 1);
    }

    drawAxis(W, domain, tz);
  }

  // 2 to 8 TZ-aware ticks, aligned to the canvas left edge via
  // --heatmap-label-w. When the visible span crosses a calendar-day
  // boundary, ticks carry the date ("YYYY-MM-DD HH:MM:SS") - time-only
  // ticks across a multi-day span were ambiguous. Tick COUNT is unchanged
  // in both modes.
  function drawAxis(W: number, domain: TimeDomain, tz: boolean): void {
    els.heatmapAxis.textContent = "";
    const { tMin, tMax } = domain;
    const withDate = crossesDayBoundary(tMin, tMax, tz);
    const n = clamp(Math.floor(W / 130), 2, 8);
    for (let i = 0; i <= n; i++) {
      const frac = i / n;
      const tick = document.createElement("div");
      tick.className = "tick";
      tick.style.left = LABEL_W() + frac * W + "px";
      tick.textContent = fmtTick(tMin + frac * (tMax - tMin), tz, withDate);
      els.heatmapAxis.appendChild(tick);
    }
  }

  store.subscribe(["browse", "ui"], (state) => {
    assertInScheduledRender("browse-view render");
    const b = state.browse;

    // Truncation banner.
    if (b.warning) {
      els.browseWarning.textContent = "⚠ " + b.warning;
      els.browseWarning.style.display = "";
    } else {
      els.browseWarning.textContent = "";
      els.browseWarning.style.display = "none";
    }

    // Status area.
    renderStatus(els.browseStatus, b.status);

    // Heatmap visibility BEFORE painting: the canvas width is measured from
    // the live layout, so the container must be displayed first.
    els.heatmapView.style.display = b.heatmapVisible ? "" : "none";

    // The Reset zoom control shows only while a sub-range is displayed.
    const zoomed =
      b.fullDomain &&
      b.domain &&
      (b.domain.tMin !== b.fullDomain.tMin || b.domain.tMax !== b.fullDomain.tMax);
    els.heatmapResetZoom.style.display = zoomed ? "" : "none";

    if (!b.heatmapVisible || !b.rows.length || !b.domain) return;

    if (b.rows !== lastRows) rebuildLabels(b.rows);
    if (
      b.rows !== lastRows ||
      b.domain !== lastDomain ||
      state.ui.useLocalTz !== lastTz ||
      b.renderEpoch !== lastEpoch
    ) {
      drawCanvas(b.rows, b.domain, state.ui.useLocalTz);
      lastRows = b.rows;
      lastDomain = b.domain;
      lastTz = state.ui.useLocalTz;
      lastEpoch = b.renderEpoch;
    }
  });
}
