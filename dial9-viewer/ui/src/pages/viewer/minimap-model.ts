// The overview-minimap density/coverage model + geometry. PURE and
// Node-testable: no DOM, no canvas, no store.
//
// The minimap is the viewer's position context: a compressed overview of the
// WHOLE trace time range with a draggable box marking the visible window. Its
// density comes from tier-1 sources only, so a multi-segment trace whose tail
// is UNFETCHED is still navigable - the tail renders from listing-metadata
// extents, never as empty/complete.
//
// Density source precedence:
//   1. aggregate density with coverage === "full" -> trust it (server-folded,
//      complete);
//   2. otherwise fall back to LISTING-METADATA density (segment extents + gzip
//      sizes from the segments slice) - do NOT present a partial aggregate as
//      complete;
//   3. whole-trace (non-segmented, empty segments slice) -> the
//      component-supplied per-time event histogram, all resident/complete;
//   4. nothing loaded -> null range, the minimap renders empty.
//
// Per-bin COVERAGE distinguishes fetched ("parsed") from tier-1-only regions,
// mapping segment residency exactly as the at-cursor readout does so the two
// surfaces agree: parsed -> complete; listed/fetching/evicted -> truncated;
// oversized -> oversized; no covering segment -> empty (a listing gap).

import type { CoverageSignal } from "../../lib/trace/index.js";
import type { SegmentEntry, SegmentLifecycle } from "../../types/state.js";

/** The overview range the minimap spans (trace-monotonic ns). */
export interface MinimapRange {
  startNs: number;
  endNs: number;
}

/** A visible time window (the viewport slice's [viewStart, viewEnd]). */
export interface MinimapWindow {
  viewStart: number;
  viewEnd: number;
}

/**
 * Per-bin coverage. "empty" is a listing GAP (a segmented trace with no
 * segment covering this bin - never fabricated data); the others map segment
 * residency (see module header). Distinct from CoverageSignal (which is the
 * overall badge axis).
 */
export type BinCoverage = "complete" | "truncated" | "oversized" | "empty";

/** One overview bin: a normalized bar height + its coverage state. */
export interface DensityBin {
  /** Normalized density in [0, 1] for the bar height. */
  density: number;
  coverage: BinCoverage;
}

/** The current viewport as a fraction of the overview range (for the box). */
export interface ViewportBox {
  /** Left edge, [0, 1]. */
  leftFrac: number;
  /** Width, (0, 1]. */
  widthFrac: number;
}

/**
 * Aggregate density (tier 1). `bins` is a normalized density series of any
 * length (resampled to the minimap's bin count); `coverage` is a
 * CoverageSignal - only trusted for the density when "full" (else the model
 * falls back to listing metadata).
 */
export interface AggregateDensity {
  coverage: CoverageSignal;
  bins: readonly number[];
}

/** Inputs to computeDensityBins (all tier-1 sources). */
export interface DensityInputs {
  range: MinimapRange;
  /** Segments slice: extents (tier 1) + residency (tier 2 when parsed). */
  segments: ReadonlyMap<string, SegmentEntry>;
  /** Aggregate density; used only when coverage === "full". */
  aggregate?: AggregateDensity | null;
  /** Whole-trace event histogram (component-derived), used when no segments. */
  traceDensity?: readonly number[] | null;
  /** Number of overview bins to produce. */
  binCount: number;
}

/** The assembled minimap model for one render pass. */
export interface MinimapModel {
  range: MinimapRange;
  bins: DensityBin[];
  /** Overall coverage descriptor for the badge (see overallCoverage). */
  coverage: CoverageSignal;
  /** True when any in-range bin is tier-1-only (truncated/oversized). */
  hasTier1Only: boolean;
  viewportBox: ViewportBox;
}

// ── Geometry (ns <-> fraction) ───────────────────────────────────────────

function clamp01(v: number): number {
  if (v < 0) return 0;
  if (v > 1) return 1;
  return v;
}

/** Map a timestamp to its fraction of the range, clamped to [0, 1]. */
export function nsToFrac(range: MinimapRange, ns: number): number {
  const span = range.endNs - range.startNs;
  if (span <= 0) return 0;
  return clamp01((ns - range.startNs) / span);
}

/** Map a fraction [0, 1] back to a timestamp within the range. */
export function fracToNs(range: MinimapRange, frac: number): number {
  const span = range.endNs - range.startNs;
  return range.startNs + clamp01(frac) * span;
}

/**
 * The overview range: the union of the navigable viewport bounds
 * [minTs, maxTs] and every listed segment extent, so a tier-1-only tail
 * (segment extents beyond the resident trace's bounds - a transitional
 * partial-bounds state) stays inside the range and thus navigable. Returns
 * null when nothing is loaded (degenerate bounds and no segments).
 */
export function deriveMinimapRange(
  viewport: { minTs: number; maxTs: number },
  segments: ReadonlyMap<string, SegmentEntry>,
): MinimapRange | null {
  let start = viewport.minTs;
  let end = viewport.maxTs;
  let has = viewport.maxTs > viewport.minTs;
  for (const seg of segments.values()) {
    if (!has) {
      start = seg.extent.startNs;
      end = seg.extent.endNs;
      has = true;
      continue;
    }
    if (seg.extent.startNs < start) start = seg.extent.startNs;
    if (seg.extent.endNs > end) end = seg.extent.endNs;
  }
  if (!has || end <= start) return null;
  return { startNs: start, endNs: end };
}

// ── Coverage mapping ─────────────────────────────────────────────────────

/**
 * Map one segment's residency to a bin coverage, exactly as the at-cursor
 * readout maps it, so the minimap and the readout never disagree about the
 * same segment.
 */
export function segmentBinCoverage(state: SegmentLifecycle): Exclude<BinCoverage, "empty"> {
  switch (state) {
    case "oversized":
      return "oversized";
    case "parsed":
      return "complete";
    case "listed":
    case "fetching":
    case "evicted":
      return "truncated";
  }
}

/** Escalate to the worst of two coverages (oversized > truncated > complete > empty). */
function worseCoverage(a: BinCoverage, b: BinCoverage): BinCoverage {
  const rank: Record<BinCoverage, number> = {
    empty: 0,
    complete: 1,
    truncated: 2,
    oversized: 3,
  };
  return rank[b] > rank[a] ? b : a;
}

// ── Density bins ─────────────────────────────────────────────────────────

/**
 * Bin a set of timestamps into `resolution` counts over `range` (the
 * whole-trace event histogram source, component-derived once per trace). Out
 * of range timestamps are dropped. Returns raw counts (computeDensityBins
 * normalizes on resample).
 */
export function binTimestamps(
  times: readonly number[],
  range: MinimapRange,
  resolution: number,
): number[] {
  const span = range.endNs - range.startNs;
  if (span <= 0 || resolution <= 0) return [];
  const bins = new Array<number>(resolution).fill(0);
  for (const t of times) {
    if (t < range.startNs || t > range.endNs) continue;
    let idx = Math.floor(((t - range.startNs) / span) * resolution);
    if (idx >= resolution) idx = resolution - 1;
    if (idx < 0) idx = 0;
    bins[idx] = (bins[idx] ?? 0) + 1;
  }
  return bins;
}

/** Resample a normalized source series to `binCount` bins (nearest-neighbor). */
function resample(src: readonly number[], binCount: number): number[] {
  const max = src.reduce((m, v) => (v > m ? v : m), 0) || 1;
  const out: number[] = [];
  for (let i = 0; i < binCount; i++) {
    const j = Math.min(src.length - 1, Math.floor((i / binCount) * src.length));
    out.push(clamp01((src[j] ?? 0) / max));
  }
  return out;
}

/**
 * Compute the overview density bins (see module header for the source
 * precedence). Always returns exactly `binCount` bins for a non-degenerate
 * range; an empty array for a zero-width range or non-positive binCount.
 */
export function computeDensityBins(input: DensityInputs): DensityBin[] {
  const { range, segments, aggregate, traceDensity, binCount } = input;
  const span = range.endNs - range.startNs;
  if (span <= 0 || binCount <= 0) return [];

  // 1. Aggregate density, trusted only when fully folded.
  if (aggregate != null && aggregate.coverage === "full" && aggregate.bins.length > 0) {
    return resample(aggregate.bins, binCount).map((density) => ({
      density,
      coverage: "complete" as BinCoverage,
    }));
  }

  // 2/3. Listing-metadata density (segments), else the whole-trace histogram.
  const binNs = span / binCount;
  const rawBytes = new Array<number>(binCount).fill(0);
  const cov: BinCoverage[] = new Array<BinCoverage>(binCount).fill("empty");

  if (segments.size > 0) {
    for (const seg of segments.values()) {
      const segSpan = seg.extent.endNs - seg.extent.startNs;
      // Byte rate over the segment's own extent; a degenerate extent still
      // deposits its whole size into the single bin it lands in.
      const rate = segSpan > 0 ? seg.sizeBytes / segSpan : seg.sizeBytes;
      const lo = Math.max(0, Math.floor((seg.extent.startNs - range.startNs) / binNs));
      const hi = Math.min(binCount - 1, Math.floor((seg.extent.endNs - range.startNs) / binNs));
      const segCov = segmentBinCoverage(seg.state);
      for (let b = lo; b <= hi; b++) {
        const binStart = range.startNs + b * binNs;
        const binEnd = binStart + binNs;
        const overlap =
          Math.min(seg.extent.endNs, binEnd) - Math.max(seg.extent.startNs, binStart);
        if (segSpan > 0) {
          rawBytes[b] = (rawBytes[b] ?? 0) + rate * Math.max(0, overlap);
        } else {
          rawBytes[b] = (rawBytes[b] ?? 0) + seg.sizeBytes;
        }
        cov[b] = worseCoverage(cov[b] ?? "empty", segCov);
      }
    }
    const max = rawBytes.reduce((m, v) => (v > m ? v : m), 0) || 1;
    return rawBytes.map((bytes, b) => ({
      density: clamp01(bytes / max),
      coverage: cov[b] ?? "empty",
    }));
  }

  // 3. Whole-trace (non-segmented) path: the event histogram is fully
  // resident, so every bin is complete. With no histogram, a flat band still
  // gives position context.
  if (traceDensity != null && traceDensity.length > 0) {
    return resample(traceDensity, binCount).map((density) => ({
      density: Math.max(density, 0.04),
      coverage: "complete" as BinCoverage,
    }));
  }
  return new Array<DensityBin>(binCount).fill({ density: 0.5, coverage: "complete" });
}

/**
 * The overall coverage descriptor the badge surfaces (never silently wrong):
 * the aggregate signal when present, else "partial" when any in-range region
 * is tier-1-only, "full" when everything is resident/complete, "none" when
 * nothing is loaded.
 */
export function overallCoverage(
  bins: readonly DensityBin[],
  aggregate: AggregateDensity | null | undefined,
): { signal: CoverageSignal; hasTier1Only: boolean } {
  const hasTier1Only = bins.some(
    (b) => b.coverage === "truncated" || b.coverage === "oversized",
  );
  if (aggregate != null) return { signal: aggregate.coverage, hasTier1Only };
  if (bins.length === 0) return { signal: "none", hasTier1Only };
  return { signal: hasTier1Only ? "partial" : "full", hasTier1Only };
}

/** The viewport-box rect as fractions of the overview range. */
export function viewportBox(
  range: MinimapRange,
  viewStart: number,
  viewEnd: number,
): ViewportBox {
  const leftFrac = nsToFrac(range, viewStart);
  const rightFrac = nsToFrac(range, viewEnd);
  return { leftFrac, widthFrac: Math.max(rightFrac - leftFrac, 0) };
}

/** Assemble the full model for one render pass. */
export function buildMinimapModel(
  range: MinimapRange,
  viewport: MinimapWindow,
  input: Omit<DensityInputs, "range">,
): MinimapModel {
  const bins = computeDensityBins({ ...input, range });
  const { signal, hasTier1Only } = overallCoverage(bins, input.aggregate ?? null);
  return {
    range,
    bins,
    coverage: signal,
    hasTier1Only,
    viewportBox: viewportBox(range, viewport.viewStart, viewport.viewEnd),
  };
}

// ── Pointer -> viewport navigation (drag + click), pure ──────────────────

/** Clamp a [start, start+width] window into the range, preserving width. */
function clampWindow(range: MinimapRange, start: number, width: number): MinimapWindow {
  const w = Math.max(0, width);
  let s = start;
  let e = s + w;
  if (s < range.startNs) {
    s = range.startNs;
    e = s + w;
  }
  if (e > range.endNs) {
    e = range.endNs;
    s = e - w;
  }
  // A view wider than the whole range pins to the start (can't fit anyway).
  if (s < range.startNs) s = range.startNs;
  return { viewStart: s, viewEnd: s + w };
}

/**
 * Click navigation: center the current view (width preserved) on `targetNs`,
 * clamped to the range. A click on the tier-1-only tail jumps the viewport
 * there.
 */
export function minimapClickWindow(
  range: MinimapRange,
  viewWidth: number,
  targetNs: number,
): MinimapWindow {
  return clampWindow(range, targetNs - viewWidth / 2, viewWidth);
}

/**
 * Drag navigation: place the box so the point grabbed at drag start
 * (`grabOffsetNs` from the box's left edge) stays under `pointerNs`, width
 * preserved, clamped to the range. `grabOffsetNs` is captured once at
 * pointer-down (see grabOffsetFor) so the box does not jump under the cursor.
 */
export function minimapDragWindow(
  range: MinimapRange,
  viewWidth: number,
  grabOffsetNs: number,
  pointerNs: number,
): MinimapWindow {
  return clampWindow(range, pointerNs - grabOffsetNs, viewWidth);
}

/** Whether a timestamp falls inside the current viewport window. */
export function insideViewport(window: MinimapWindow, ns: number): boolean {
  return ns >= window.viewStart && ns <= window.viewEnd;
}

/**
 * The grab offset to capture at pointer-down: the pointer's distance from the
 * box's left edge when it lands INSIDE the box (scrub without jumping), else
 * half the width (an outside click centers the box on the cursor, then drags
 * keep it centered).
 */
export function grabOffsetFor(
  window: MinimapWindow,
  viewWidth: number,
  pointerNs: number,
): number {
  if (insideViewport(window, pointerNs)) return pointerNs - window.viewStart;
  return viewWidth / 2;
}

/**
 * POI-tick occupancy per pixel column: `cols[x] === 1` iff some POI maps to
 * column `x` (`Math.floor(nsToFrac(range, time) * cssW)`, the exact pixel the old
 * per-POI tick loop drew). The minimap range is the whole trace and cssW is
 * stable across pans, so the caller caches this once and repaints the columns in
 * O(cssW) - instead of an O(pois) fillRect loop every frame (pois can be 100K+).
 * The array has `cssW + 1` slots so a POI at endNs (frac 1 -> column cssW) never
 * indexes out of range; that column sits off the visible width, matching the old
 * off-edge draw.
 */
export function poiTickColumns(
  range: MinimapRange,
  pois: readonly { time: number }[],
  cssW: number,
): Uint8Array {
  const cols = new Uint8Array(Math.max(0, cssW) + 1);
  if (cssW <= 0 || range.endNs <= range.startNs) return cols;
  for (const p of pois) {
    if (p.time < range.startNs || p.time > range.endNs) continue;
    cols[Math.floor(nsToFrac(range, p.time) * cssW)] = 1;
  }
  return cols;
}
