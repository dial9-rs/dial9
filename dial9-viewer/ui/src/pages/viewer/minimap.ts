// The overview minimap canvas component: fills the shell's `.d9-minimap` slot
// with a compressed whole-trace overview - tier-1 density (segment listing
// extents + aggregate density), POI ticks, and a draggable viewport box.
//
// Store-wired: it subscribes to trace / viewport / segments and redraws its own
// canvas each frame, inside the store's notification tick. Drag/click on the
// canvas dispatch a store viewport update (the pure navigation math is in
// minimap-model) - never a direct render. The canvas is (re)ensured every draw
// so a shell re-render that reconciles the `.d9-minimap` host cannot orphan it.
//
// Coverage honesty: tier-1-only regions (unfetched / evicted / oversized
// segments) render in a distinct dimmed style and drive a "partial" badge, so
// the overview never presents a windowed tail as whole.

import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import { createCanvasSizer, type CanvasSizer } from "../../lib/canvas/dpr.js";
import { ColumnarEvents } from "../../lib/trace/columnar-events.js";
import {
  binTimestamps,
  buildMinimapModel,
  deriveMinimapRange,
  fracToNs,
  grabOffsetFor,
  minimapClickWindow,
  minimapDragWindow,
  poiTickColumns,
  type AggregateDensity,
  type BinCoverage,
  type MinimapModel,
  type MinimapRange,
} from "./minimap-model.js";
import { deriveMinimapPois, type MinimapPoi } from "./minimap-poi.js";

const CANVAS_CLASS = "d9-minimap-canvas";
const BADGE_CLASS = "d9-minimap-badge";

/** Whole-trace histogram resolution (resampled to bin count at render). */
const TRACE_DENSITY_RESOLUTION = 256;

/** Colors, aligned with viewer.css's palette (kept literal for the 2D ctx). */
const COLORS: Record<BinCoverage, string> = {
  complete: "#5b8fb0",
  truncated: "rgba(129,138,178,0.5)",
  oversized: "#c9736b",
  empty: "rgba(129,138,178,0.12)",
};
const POI_TICK = "#ffca5f";
const BOX_FILL = "rgba(143,136,255,0.18)";
const BOX_BORDER = "#8f88ff";

export interface MinimapDeps {
  /**
   * Aggregate density supplier. Returns null when no aggregate data is wired -
   * the model then falls back to listing-metadata / whole-trace density. Kept
   * a seam so aggregates can be fed later without touching the component.
   */
  aggregate?(): AggregateDensity | null;
}

export interface MountedMinimap {
  dispose(): void;
}

/** Dispatch a viewport window to the store (the navigation commit). */
export function minimapNavigate(
  store: ViewerStore,
  range: MinimapRange,
  viewWidth: number,
  mode:
    | { kind: "click"; ns: number }
    | { kind: "drag"; grabOffsetNs: number; ns: number },
): void {
  const win =
    mode.kind === "click"
      ? minimapClickWindow(range, viewWidth, mode.ns)
      : minimapDragWindow(range, viewWidth, mode.grabOffsetNs, mode.ns);
  store.update("viewport", { viewStart: win.viewStart, viewEnd: win.viewEnd });
}

/**
 * Mount the overview minimap against `store`, drawing into `region` (the
 * shell's `.d9-minimap` host). Returns teardown handles.
 */
export function mountMinimap(
  region: HTMLElement,
  store: ViewerStore,
  deps: MinimapDeps = {},
): MountedMinimap {
  const doc = region.ownerDocument;

  // Frame-invariant derivations, recomputed only when the trace slice is
  // replaced: POI ticks and the whole-trace event histogram.
  const pois = store.derived(["trace"], (s): MinimapPoi[] =>
    s.trace.trace ? deriveMinimapPois(s.trace.trace) : [],
  );
  const traceDensity = store.derived(["trace"], (s): number[] | null => {
    const t = s.trace.trace;
    if (t === null || t.minTs === null || t.maxTs === null || t.maxTs <= t.minTs) {
      return null;
    }
    // Columnar path: bin straight off the ts column (bin-for-bin identical to the
    // fat path below, but without the ~13.7M-number `.map(e => e.timestamp)`
    // transient - the whole point of the columnar store).
    if (t.events instanceof ColumnarEvents) {
      return t.events.densityBins(t.minTs, t.maxTs, TRACE_DENSITY_RESOLUTION);
    }
    const times = t.events.map((e) => e.timestamp);
    return binTimestamps(times, { startNs: t.minTs, endNs: t.maxTs }, TRACE_DENSITY_RESOLUTION);
  });

  let sizer: CanvasSizer<CanvasRenderingContext2D> | null = null;
  let sizerCanvas: HTMLCanvasElement | null = null;

  // POI ticks bucketed to pixel columns. The minimap range is the whole trace
  // (deriveMinimapRange reads viewport.min/maxTs, the fixed extent) and cssW is
  // stable across pans, so this is a cache HIT every pan frame - turning an
  // O(pois) per-frame fillRect loop (pois can be 100K+ on a big trace) into an
  // O(cssW) column paint. Recomputed only when the poi set, range, or width
  // actually change (trace load, streaming extent, resize).
  let poiColCache:
    | { pois: readonly MinimapPoi[]; startNs: number; endNs: number; cssW: number; cols: Uint8Array }
    | null = null;
  function poiColumns(range: MinimapRange, ps: readonly MinimapPoi[], cssW: number): Uint8Array {
    const c = poiColCache;
    if (c && c.pois === ps && c.startNs === range.startNs && c.endNs === range.endNs && c.cssW === cssW) {
      return c.cols;
    }
    const cols = poiTickColumns(range, ps, cssW);
    poiColCache = { pois: ps, startNs: range.startNs, endNs: range.endNs, cssW, cols };
    return cols;
  }

  /** Create the canvas + badge once and keep them attached (idempotent). */
  function ensureCanvas(): HTMLCanvasElement {
    let canvas = region.querySelector<HTMLCanvasElement>(`.${CANVAS_CLASS}`);
    if (!canvas) {
      canvas = doc.createElement("canvas");
      canvas.className = CANVAS_CLASS;
      canvas.setAttribute("role", "img");
      canvas.addEventListener("mousedown", onMouseDown);
      region.appendChild(canvas);
    }
    if (canvas !== sizerCanvas) {
      sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      sizerCanvas = canvas;
    }
    if (!region.querySelector(`.${BADGE_CLASS}`)) {
      const badge = doc.createElement("span");
      badge.className = BADGE_CLASS;
      badge.hidden = true;
      region.appendChild(badge);
    }
    return canvas;
  }

  function badgeEl(): HTMLElement | null {
    return region.querySelector<HTMLElement>(`.${BADGE_CLASS}`);
  }

  /** Current overview range from store state, or null when nothing is loaded. */
  function currentRange(state: StoreState): MinimapRange | null {
    return deriveMinimapRange(state.viewport, state.segments.segments);
  }

  function buildModel(state: StoreState, binCount: number): MinimapModel | null {
    const range = currentRange(state);
    if (range === null) return null;
    return buildMinimapModel(
      range,
      { viewStart: state.viewport.viewStart, viewEnd: state.viewport.viewEnd },
      {
        segments: state.segments.segments,
        aggregate: deps.aggregate?.() ?? null,
        traceDensity: traceDensity(),
        binCount,
      },
    );
  }

  function draw(): void {
    const canvas = ensureCanvas();
    const state = store.getState();
    const rect = canvas.getBoundingClientRect();
    const cssW = Math.round(rect.width);
    const cssH = Math.round(rect.height);
    if (cssW <= 0 || cssH <= 0) return;
    const dpr = (typeof devicePixelRatio === "number" ? devicePixelRatio : 1) || 1;
    if (sizer === null) return;
    const ctx = sizer.ensure(cssW, cssH, dpr);

    ctx.clearRect(0, 0, cssW, cssH);
    ctx.fillStyle = "#0f1226";
    ctx.fillRect(0, 0, cssW, cssH);

    const binCount = Math.max(1, Math.min(cssW, 480));
    const model = buildModel(state, binCount);
    const badge = badgeEl();
    if (model === null) {
      canvas.setAttribute("aria-label", "Overview minimap: no trace loaded");
      if (badge) badge.hidden = true;
      return;
    }

    drawDensity(ctx, model, cssW, cssH);
    drawPoiTicks(ctx, poiColumns(model.range, pois(), cssW), cssW);
    drawViewportBox(ctx, model, cssW, cssH);
    updateBadge(badge, model);

    const { range } = model;
    const total = (range.endNs - range.startNs) / 1e9;
    const vs = (state.viewport.viewStart - range.startNs) / 1e9;
    const ve = (state.viewport.viewEnd - range.startNs) / 1e9;
    canvas.setAttribute(
      "aria-label",
      `Overview minimap: viewing +${vs.toFixed(2)}s to +${ve.toFixed(2)}s ` +
        `of ${total.toFixed(2)}s total${model.hasTier1Only ? " (partial data - some regions unfetched)" : ""}`,
    );
  }

  // ── pointer navigation (drag + click), the pure math is in the model ────

  let dragGrabOffsetNs: number | null = null;
  let dragWidth = 0;

  function nsAtClientX(canvas: HTMLElement, range: MinimapRange, clientX: number): number {
    const rect = canvas.getBoundingClientRect();
    const frac = rect.width > 0 ? (clientX - rect.left) / rect.width : 0;
    return fracToNs(range, frac);
  }

  function onMouseDown(e: MouseEvent): void {
    const state = store.getState();
    const range = currentRange(state);
    if (range === null) return;
    e.preventDefault();
    const canvas = e.currentTarget as HTMLElement;
    const ns = nsAtClientX(canvas, range, e.clientX);
    const vp = state.viewport;
    dragWidth = Math.max(0, vp.viewEnd - vp.viewStart);
    dragGrabOffsetNs = grabOffsetFor(
      { viewStart: vp.viewStart, viewEnd: vp.viewEnd },
      dragWidth,
      ns,
    );
    // A press jumps immediately (click-to-navigate); the ensuing drag scrubs.
    minimapNavigate(store, range, dragWidth, { kind: "drag", grabOffsetNs: dragGrabOffsetNs, ns });
    doc.addEventListener("mousemove", onMouseMove);
    doc.addEventListener("mouseup", onMouseUp);
  }

  function onMouseMove(e: MouseEvent): void {
    if (dragGrabOffsetNs === null) return;
    const state = store.getState();
    const range = currentRange(state);
    const canvas = region.querySelector<HTMLElement>(`.${CANVAS_CLASS}`);
    if (range === null || canvas === null) return;
    const ns = nsAtClientX(canvas, range, e.clientX);
    minimapNavigate(store, range, dragWidth, { kind: "drag", grabOffsetNs: dragGrabOffsetNs, ns });
  }

  function onMouseUp(): void {
    dragGrabOffsetNs = null;
    doc.removeEventListener("mousemove", onMouseMove);
    doc.removeEventListener("mouseup", onMouseUp);
  }

  // Keyboard nav on the focusable region (the region carries tabindex=0): pan
  // the view by a fraction of its width so the overview is not a focus trap.
  function onKeyDown(e: KeyboardEvent): void {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    const state = store.getState();
    const range = currentRange(state);
    if (range === null) return;
    const vp = state.viewport;
    const width = Math.max(0, vp.viewEnd - vp.viewStart);
    if (width <= 0) return;
    const center = (vp.viewStart + vp.viewEnd) / 2;
    const step = width * 0.15 * (e.key === "ArrowLeft" ? -1 : 1);
    e.preventDefault();
    minimapNavigate(store, range, width, { kind: "click", ns: center + step });
  }
  region.addEventListener("keydown", onKeyDown);

  const unsubscribe = store.subscribe(["trace", "viewport", "segments"], () => draw());
  draw();

  return {
    dispose(): void {
      unsubscribe();
      region.removeEventListener("keydown", onKeyDown);
      doc.removeEventListener("mousemove", onMouseMove);
      doc.removeEventListener("mouseup", onMouseUp);
      const canvas = region.querySelector<HTMLElement>(`.${CANVAS_CLASS}`);
      canvas?.removeEventListener("mousedown", onMouseDown);
      canvas?.remove();
      badgeEl()?.remove();
    },
  };
}

// ── drawing helpers ───────────────────────────────────────────────────────

function drawDensity(
  ctx: CanvasRenderingContext2D,
  model: MinimapModel,
  cssW: number,
  cssH: number,
): void {
  const n = model.bins.length;
  if (n === 0) return;
  const barW = cssW / n;
  // A small top margin leaves room for POI ticks; bars grow from the baseline.
  const top = 6;
  const usableH = Math.max(1, cssH - top - 1);
  for (let i = 0; i < n; i++) {
    const bin = model.bins[i]!;
    if (bin.coverage === "empty" && bin.density <= 0) continue;
    const h = Math.max(1, bin.density * usableH);
    ctx.fillStyle = COLORS[bin.coverage];
    ctx.fillRect(i * barW, cssH - h - 1, Math.max(1, barW + 0.5), h);
  }
  // Baseline rule.
  ctx.strokeStyle = "rgba(255,255,255,0.08)";
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, cssH - 0.5);
  ctx.lineTo(cssW, cssH - 0.5);
  ctx.stroke();
}

function drawPoiTicks(
  ctx: CanvasRenderingContext2D,
  cols: Uint8Array,
  cssW: number,
): void {
  ctx.fillStyle = POI_TICK;
  // One 1px tick per occupied pixel column (bucketed + cached upstream), so this
  // is O(cssW) regardless of how many POIs the trace has.
  for (let x = 0; x < cssW; x++) {
    if (cols[x]) ctx.fillRect(x, 0, 1, 5);
  }
}

function drawViewportBox(
  ctx: CanvasRenderingContext2D,
  model: MinimapModel,
  cssW: number,
  cssH: number,
): void {
  const { leftFrac, widthFrac } = model.viewportBox;
  const x = leftFrac * cssW;
  const w = Math.max(2, widthFrac * cssW);
  const clampedX = Math.min(x, Math.max(0, cssW - w));
  ctx.fillStyle = BOX_FILL;
  ctx.fillRect(clampedX, 0, w, cssH);
  ctx.strokeStyle = BOX_BORDER;
  ctx.lineWidth = 1;
  ctx.strokeRect(clampedX + 0.5, 0.5, w - 1, cssH - 1);
}

function updateBadge(badge: HTMLElement | null, model: MinimapModel): void {
  if (badge === null) return;
  if (model.hasTier1Only) {
    badge.hidden = false;
    badge.textContent = "partial";
    badge.title =
      "Some regions are not fetched (tier-1 overview only); pan into them to load.";
    badge.dataset["coverage"] = model.coverage;
  } else if (model.coverage === "partial") {
    badge.hidden = false;
    badge.textContent = "partial";
    badge.title = "Aggregate coverage is still folding; density may be incomplete.";
    badge.dataset["coverage"] = model.coverage;
  } else {
    badge.hidden = true;
    badge.removeAttribute("data-coverage");
  }
}
