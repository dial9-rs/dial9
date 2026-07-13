// The Process CPU usage track: a fillRect BAR chart of avg-cores-over-time
// (not a curve). Bars go through the run-length coalescer; the grid lines and
// the dashed capacity line go through the batched-stroke path (one path per
// style) - the two paths are never mixed. Draws in draw-area-relative x
// (nsToDrawX, no LABEL_W) so bars line up pixel-exact with the lanes/axis.
// `renderCpuTrack` returns the top-right info readout so the shell can mirror
// it into a DOM-queryable attribute; the hover CONTENT (cpuIntervalAt +
// cpuIntervalTooltip) is consumed by the overlay/tooltip system.
//
// Windowing: when the data is segment-windowed the renderer must NOT paint a
// truncated window as complete. `renderCpuTrack` consumes a `CpuWindow`
// descriptor and surfaces truncation (an edge hatch band) + the `oversized`
// segment state (a "partial window" badge). Whole-trace loads resolve the
// descriptor to "complete".

import type { PanelGeometry, StoreState } from "../../types/state.js";
import type { ParsedTrace } from "../../types/trace.js";
import {
  buildProcessCpuUsageSeries,
  formatHumanDuration,
  type ProcessCpuUsageInterval,
} from "../../lib/trace/index.js";
import {
  makeBarCoalescer,
  makeStrokeBatcher,
  drawStrokeBatches,
  type StrokeStyleSpec,
} from "../../lib/canvas/index.js";

// ── Windowing descriptor ─────────────────────────────────────────────────

/**
 * The resident-data-window state the CPU renderer must surface so a
 * truncated/partial window is never painted as complete. Both fields resolve
 * to the "complete" value ({ truncatedAt: null, oversized: false }) for a
 * whole-trace load.
 */
export interface CpuWindow {
  /**
   * Which edge(s) of the resident window truncate the CPU data: data beyond
   * the edge is not resident, so that edge is a WINDOW boundary, not the true
   * data boundary. null when the window covers the whole trace.
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
export const COMPLETE_WINDOW: CpuWindow = { truncatedAt: null, oversized: false };

// ── Store-lifted inputs ─────────────────────────────────────────────────

/**
 * Everything the CPU track needs for one render pass, lifted out of the
 * store so the pure logic (stats, formatting, render) stays Node-testable.
 * `deriveCpuInputs` builds it from a store snapshot.
 */
export interface CpuInputs {
  /**
   * The avg-cores-over-time intervals (`buildProcessCpuUsageSeries`), sorted
   * ascending by `start` and contiguous except where a sample was dropped.
   * Memoized on the trace identity - see `cpuSeriesFor`.
   */
  intervals: readonly ProcessCpuUsageInterval[];
  /**
   * Available parallelism = the CPU "capacity", the normalized
   * `process.available_parallelism` segment metadatum; null when unknown
   * (no capacity line, no percent readout).
   */
  capacity: number | null;
  /** Resident-window truncation state. */
  window: CpuWindow;
}

// buildProcessCpuUsageSeries scans every custom event, so memoize on the
// ParsedTrace identity: the trace slice is replaced wholesale on load, so a
// new trace object is a real cache miss and a stable one (pan/zoom/selection
// frames) is a hit.
type CpuSeries = ReturnType<typeof buildProcessCpuUsageSeries>;
const seriesCache = new WeakMap<ParsedTrace, CpuSeries>();

/** The memoized CPU series for a trace (built once per loaded trace). */
export function cpuSeriesFor(trace: ParsedTrace): CpuSeries {
  let series = seriesCache.get(trace);
  if (series === undefined) {
    // segmentMetadata carries `process.available_parallelism` as a string;
    // buildProcessCpuUsageSeries normalizes it (numericField, >0 or null).
    const capRaw = trace.segmentMetadata?.get("process.available_parallelism") ?? null;
    series = buildProcessCpuUsageSeries(trace.customEvents ?? [], capRaw);
    seriesCache.set(trace, series);
  }
  return series;
}

/**
 * Derive the CPU window descriptor from the store. The `oversized` state comes
 * directly from the segments slice - any needed segment stuck in "oversized"
 * makes the view partial. `truncatedAt` stays null on the whole-trace path
 * (empty segments slice); the renderer consumes the descriptor regardless.
 */
export function deriveCpuWindow(state: StoreState): CpuWindow {
  let oversized = false;
  for (const entry of state.segments.segments.values()) {
    if (entry.state === "oversized") {
      oversized = true;
      break;
    }
  }
  return { truncatedAt: null, oversized };
}

/** Lift the CPU track's inputs from a store snapshot. */
export function deriveCpuInputs(state: StoreState): CpuInputs {
  const trace = state.trace.trace;
  if (trace === null) {
    return { intervals: [], capacity: null, window: COMPLETE_WINDOW };
  }
  const series = cpuSeriesFor(trace);
  return {
    intervals: series.intervals,
    capacity: series.availableParallelism,
    window: deriveCpuWindow(state),
  };
}

// ── Visible-window stats ─────────────────────────────────────────────────

export interface CpuStats {
  /** Time-weighted mean cores over the visible window (0 when no overlap). */
  avgCores: number;
  /** Peak interval cores touching the visible window. */
  maxCores: number;
}

/**
 * The avg/max readout stats for the visible window: `avgCores` is the
 * overlap-weighted mean of `interval.cores`, `maxCores` the peak interval
 * touching the window. Intervals are sorted by start, so the loop breaks once
 * past the right edge.
 */
export function visibleCpuStats(
  intervals: readonly ProcessCpuUsageInterval[],
  viewStart: number,
  viewEnd: number,
): CpuStats {
  let maxCores = 0;
  let totalOverlapNs = 0;
  let weightedCoresNs = 0;
  for (const interval of intervals) {
    if (interval.end < viewStart) continue;
    if (interval.start > viewEnd) break;
    const overlap =
      Math.min(interval.end, viewEnd) - Math.max(interval.start, viewStart);
    if (!(overlap > 0)) continue;
    if (interval.cores > maxCores) maxCores = interval.cores;
    totalOverlapNs += overlap;
    weightedCoresNs += interval.cores * overlap;
  }
  const avgCores = totalOverlapNs > 0 ? weightedCoresNs / totalOverlapNs : 0;
  return { avgCores, maxCores };
}

// ── Formatting ───────────────────────────────────────────────────────────

/**
 * Core count -> compact label: 1 decimal at >=10 cores else 2, trailing zeros
 * and a bare dot stripped; non-finite renders `-`.
 */
export function fmtCpuCores(value: number): string {
  if (!Number.isFinite(value)) return "-";
  return value
    .toFixed(value >= 10 ? 1 : 2)
    .replace(/(\.\d*?)0+$/, "$1")
    .replace(/\.$/, "");
}

/**
 * Percent -> `NN.N%`; non-finite renders `-`.
 */
export function fmtCpuPercent(value: number): string {
  if (!Number.isFinite(value)) return "-";
  return `${value.toFixed(1)}%`;
}

// The info label joins its parts with a middle dot (U+00B7), kept as an escape
// so this source stays plain ASCII while the OUTPUT is byte-for-byte the dot.
const SEP = " · ";

/**
 * The visible-window info readout: `avg <cores> cores`, then (only when
 * capacity is known) `· avg <pct>%` where pct = min(100, avg/capacity*100),
 * then `· max <cores>`.
 */
export function cpuReadoutText(stats: CpuStats, capacity: number | null): string {
  let out = `avg ${fmtCpuCores(stats.avgCores)} cores`;
  if (capacity != null) {
    const avgPct = Math.min(100, (stats.avgCores / capacity) * 100);
    out += `${SEP}avg ${fmtCpuPercent(avgPct)}`;
  }
  out += `${SEP}max ${fmtCpuCores(stats.maxCores)}`;
  return out;
}

// ── Hover content ────────────────────────────────────────────────────────

/**
 * The interval under a timestamp, or null: binary search over the sorted,
 * non-overlapping intervals. The tooltip/crosshair system consumes this;
 * this module does not render the tooltip.
 */
export function cpuIntervalAt(
  intervals: readonly ProcessCpuUsageInterval[],
  ns: number,
): ProcessCpuUsageInterval | null {
  let lo = 0;
  let hi = intervals.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const interval = intervals[mid]!;
    if (ns < interval.start) hi = mid - 1;
    else if (ns > interval.end) lo = mid + 1;
    else return interval;
  }
  return null;
}

/** One label/value row of the CPU hover tooltip. */
export interface CpuTooltipRow {
  label: string;
  value: string;
}

/**
 * The CPU hover tooltip content as structured rows (rendered elsewhere,
 * without this module touching the DOM): Window / CPU time / Cores, plus
 * Total CPU % only when capacity is known.
 */
export function cpuIntervalTooltip(
  interval: ProcessCpuUsageInterval,
  capacity: number | null,
): CpuTooltipRow[] {
  const rows: CpuTooltipRow[] = [
    { label: "Window", value: formatHumanDuration(interval.wallDeltaNs) },
    { label: "CPU time", value: formatHumanDuration(interval.cpuDeltaNs) },
    { label: "Cores", value: fmtCpuCores(interval.cores) },
  ];
  if (capacity != null) {
    rows.push({
      label: "Total CPU",
      value: fmtCpuPercent(interval.totalPercent ?? NaN),
    });
  }
  return rows;
}

// ── Colors + geometry ────────────────────────────────────────────────────

// CPU panel colours.
const CPU_BG = "#111b2e";
const Y_LABEL = "#667";
const GRID_STROKE = "rgba(255,255,255,0.07)";
const CAP_STROKE = "rgba(255,207,153,0.65)";
const CAP_LABEL = "#ffcf99";
const READOUT_FILL = "#aaa";
const TRUNC_BAND = "rgba(255,120,120,0.28)";
const TRUNC_LABEL = "#ffb3b3";
const CAP_DASH: readonly number[] = [4, 3];

// Vertical chart margins: top headroom for labels, small bottom margin.
const CHART_TOP = 20;
const CHART_BOTTOM = 8;

const GRID_STYLE = "grid";
const CAP_STYLE = "capacity";

function strokeStyleOf(key: string): StrokeStyleSpec {
  if (key === CAP_STYLE) {
    return { strokeStyle: CAP_STROKE, lineWidth: 1, lineDash: CAP_DASH };
  }
  return { strokeStyle: GRID_STROKE, lineWidth: 1 };
}

/**
 * The bar fill colour for an interval: a blue -> pink/red ramp by load, where
 * load is `cores/capacity` (or `cores/scaleMax` when capacity is unknown), at
 * 0.42 alpha. Same load value drives the same colour (the coalescer run key,
 * so equal-load neighbours fold into one fillRect).
 */
export function cpuBarColor(
  cores: number,
  capacity: number | null,
  scaleMax: number,
): string {
  const load =
    capacity != null
      ? Math.min(1, cores / capacity)
      : Math.min(1, cores / scaleMax);
  const r = Math.round(79 + 176 * load);
  const g = Math.round(195 - 80 * load);
  const b = Math.round(247 - 150 * load);
  return `rgba(${r}, ${g}, ${b}, 0.42)`;
}

// ── Canvas render ────────────────────────────────────────────────────────

/**
 * Render the CPU track into `ctx` (already DPR-scaled and sized to
 * `geometry.time.drawW` x `geometry.height`) and RETURN the info readout
 * string (the shell mirrors it into a DOM-queryable attribute).
 *
 * Draw order: background, then the grid + dashed-capacity strokes via the
 * batched-stroke path (one path per style), then the load-coloured bars via
 * the coalescer (one fillRect per pixel run), then the axis/capacity/readout
 * text, then the window markers on top. The two render paths are never mixed.
 *
 * Called from tracks.ts `sizeTracks` for the "cpu" track, inside the store's
 * frame tick. A blank background is painted before the trace loads / when the
 * panel is too narrow; the readout is returned even then (empty-window stats)
 * so the shell stays consistent.
 */
export function renderCpuTrack(
  ctx: CanvasRenderingContext2D,
  geometry: PanelGeometry,
  viewStart: number,
  viewEnd: number,
  inputs: CpuInputs,
  hasTrace: boolean,
): string {
  const drawW = geometry.time.drawW;
  const height = geometry.height;
  const { intervals, capacity, window } = inputs;

  const stats = visibleCpuStats(intervals, viewStart, viewEnd);
  const readout = cpuReadoutText(stats, capacity);

  ctx.clearRect(0, 0, drawW, height);
  ctx.fillStyle = CPU_BG;
  ctx.fillRect(0, 0, drawW, height);
  if (!hasTrace || drawW <= 0 || viewEnd <= viewStart) return readout;

  const chartTop = CHART_TOP;
  const chartH = height - CHART_TOP - CHART_BOTTOM;
  const baselineY = chartTop + chartH;
  if (chartH <= 0) {
    // Too short to host a chart: keep the readout, skip the chart body.
    paintReadout(ctx, readout, drawW);
    return readout;
  }

  // Scale: 0 .. max(1, visible max cores, capacity).
  const scaleMax = Math.max(1, stats.maxCores, capacity ?? 0);

  // ── Strokes: 25/50/75% grid + dashed capacity, batched by style ──
  const batcher = makeStrokeBatcher();
  for (let i = 1; i <= 3; i++) {
    const y = chartTop + (chartH * i) / 4;
    batcher.polyline(GRID_STYLE, [
      { x: 0, y },
      { x: drawW, y },
    ]);
  }
  let capY: number | null = null;
  if (capacity != null) {
    capY = chartTop + chartH - (capacity / scaleMax) * chartH;
    batcher.polyline(CAP_STYLE, [
      { x: 0, y: capY },
      { x: drawW, y: capY },
    ]);
  }
  drawStrokeBatches(ctx, batcher.batches(), strokeStyleOf);

  // ── Bars: load-coloured fills via the run-length coalescer ────────────
  // Key = fill colour + pixel-rounded top, so contiguous bars of equal height
  // AND colour fold into one fillRect while a taller/shorter or
  // differently-loaded neighbour starts a new run (no height is lost). The
  // parallel runMeta map recovers the colour + top the opaque key stands for.
  const runMeta = new Map<string, { color: string; top: number }>();
  const coalescer = makeBarCoalescer((left: number, width: number, key: string) => {
    const meta = runMeta.get(key);
    if (meta === undefined) return;
    ctx.fillStyle = meta.color;
    ctx.fillRect(left, meta.top, width, baselineY - meta.top);
  });
  for (const interval of intervals) {
    if (interval.end < viewStart) continue;
    if (interval.start > viewEnd) break;
    const x1 = clampX(nsToDrawX(interval.start, viewStart, viewEnd, drawW), drawW);
    const x2 = clampX(nsToDrawX(interval.end, viewStart, viewEnd, drawW), drawW);
    const color = cpuBarColor(interval.cores, capacity, scaleMax);
    const top = Math.round(chartTop + chartH - (interval.cores / scaleMax) * chartH);
    const key = `${color}|${top}`;
    if (!runMeta.has(key)) runMeta.set(key, { color, top });
    coalescer.push(x1, x2, key);
  }
  coalescer.flush();

  // ── Text overlays: y-axis scale, capacity label, info readout ─────────
  ctx.fillStyle = Y_LABEL;
  ctx.font = "10px monospace";
  ctx.textAlign = "left";
  // y-axis scale numbers at the draw area's left edge (no in-canvas gutter).
  ctx.fillText(fmtCpuCores(scaleMax), 2, chartTop + 9);
  ctx.fillText("0", 2, baselineY);

  if (capY != null) {
    ctx.fillStyle = CAP_LABEL;
    ctx.fillText(
      `${fmtCpuCores(capacity!)} core capacity`,
      6,
      Math.max(12, capY - 4),
    );
  }

  paintReadout(ctx, readout, drawW);

  // ── Surface a truncated / oversized window (never as complete) ───
  drawWindowMarkers(ctx, window, drawW, height);

  return readout;
}

/** Draw the info readout at the draw area's top-right. */
function paintReadout(
  ctx: CanvasRenderingContext2D,
  readout: string,
  drawW: number,
): void {
  ctx.fillStyle = READOUT_FILL;
  ctx.font = "10px monospace";
  ctx.textAlign = "right";
  ctx.fillText(readout, Math.max(0, drawW - 6), 11);
}

/**
 * Surface the resident-window state: a translucent hatch band at each
 * truncated edge (so missing data reads as "not loaded", not "idle CPU"), and
 * a "partial window" badge when a needed segment is oversized. No-ops for a
 * complete window.
 */
function drawWindowMarkers(
  ctx: CanvasRenderingContext2D,
  window: CpuWindow,
  drawW: number,
  height: number,
): void {
  const BAND = 6;
  const t = window.truncatedAt;
  if (t === "start" || t === "both") {
    ctx.fillStyle = TRUNC_BAND;
    ctx.fillRect(0, 0, BAND, height);
  }
  if (t === "end" || t === "both") {
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

// ── Shared draw-area x mapping (the alignment invariant) ──────────────────

/**
 * Timestamp (ns) -> draw-area-relative x (px), identical to the axis track's
 * `nsToDrawX`: NO LABEL_W is added because the track canvas already sits
 * after the DOM label gutter, so this is the same expression every track
 * uses for its canvas-local x (the alignment invariant).
 */
export function nsToDrawX(
  ns: number,
  viewStart: number,
  viewEnd: number,
  drawW: number,
): number {
  const span = viewEnd - viewStart || 1;
  return ((ns - viewStart) / span) * drawW;
}

/** Clamp a draw-area x into [0, drawW]. */
function clampX(x: number, drawW: number): number {
  return Math.min(Math.max(x, 0), drawW);
}
