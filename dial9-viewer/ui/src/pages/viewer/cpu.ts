// The Process CPU usage track: a fillRect BAR chart of avg-cores-over-time
// (not a curve). Bars go through the run-length coalescer; their step outline,
// grid lines, and dashed capacity line go through separate batched-stroke
// paths (one path per style). Draws in draw-area-relative x (nsToDrawX, no
// LABEL_W) so bars line up pixel-exact with the lanes/axis.
// `renderCpuTrack` returns the top-right info readout so the shell can mirror
// it into the DOM header and a queryable attribute; the hover CONTENT
// (cpuIntervalAt + cpuIntervalTooltip) is consumed by the overlay/tooltip
// system.
//
// Windowing: when the data is segment-windowed the renderer must NOT paint a
// truncated window as complete. `renderCpuTrack` consumes a `CpuWindow`
// descriptor and surfaces truncation (an edge hatch band) + the `oversized`
// segment state (a "partial window" badge). Whole-trace loads resolve the
// descriptor to "complete".

import {
  COMPLETE_WINDOW,
  deriveResidentWindow as deriveCpuWindow,
  drawWindowMarkers,
  type ResidentWindow as CpuWindow,
} from "./resident-window.js";
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
  clampX,
  nsToDrawX,
} from "../../lib/canvas/index.js";

// ── Windowing descriptor ─────────────────────────────────────────────────

/**
 * The resident-data-window state the CPU renderer must surface so a
 * truncated/partial window is never painted as complete. Both fields resolve
 * to the "complete" value ({ truncatedAt: null, oversized: false }) for a
 * whole-trace load.
 */


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

function firstIntervalEndingAtOrAfter(
  intervals: readonly ProcessCpuUsageInterval[],
  ns: number,
): number {
  let lo = 0;
  let hi = intervals.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (intervals[mid]!.end < ns) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * The avg/max readout stats for the visible window: `avgCores` is the
 * overlap-weighted mean of `interval.cores`, `maxCores` the peak interval
 * touching the window. Intervals are non-overlapping + start-sorted, so `end` is
 * monotonic too: the left edge (first interval with end >= viewStart) is
 * binary-searched rather than skipped from index 0 (which grew as you pan right),
 * and the loop breaks once start passes the right edge.
 */
export function visibleCpuStats(
  intervals: readonly ProcessCpuUsageInterval[],
  viewStart: number,
  viewEnd: number,
): CpuStats {
  let maxCores = 0;
  let totalOverlapNs = 0;
  let weightedCoresNs = 0;
  for (
    let i = firstIntervalEndingAtOrAfter(intervals, viewStart);
    i < intervals.length;
    i++
  ) {
    const interval = intervals[i]!;
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
const SEP = " \u00b7 ";

/**
 * The visible-window info readout: `avg <cores> cores`, then (only when
 * capacity is known) `· avg <pct>%` where pct = min(100, avg/capacity*100),
 * then `· max <cores> cores`.
 */
export function cpuReadoutText(stats: CpuStats, capacity: number | null): string {
  let out = `avg ${fmtCpuCores(stats.avgCores)} cores`;
  if (capacity != null) {
    const avgPct = Math.min(100, (stats.avgCores / capacity) * 100);
    out += `${SEP}avg ${fmtCpuPercent(avgPct)}`;
  }
  out += `${SEP}max ${fmtCpuCores(stats.maxCores)} cores`;
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
export const CPU_CAPACITY_STROKE = "rgba(255,207,153,0.65)";
const CAP_DASH: readonly number[] = [4, 3];

// Vertical chart margins: top headroom for labels, small bottom margin.
const CHART_TOP = 20;
const CHART_BOTTOM = 8;

const GRID_STYLE = "grid";
const CAP_STYLE = "capacity";

function strokeStyleOf(key: string): StrokeStyleSpec {
  if (key === CAP_STYLE) {
    return { strokeStyle: CPU_CAPACITY_STROKE, lineWidth: 1, lineDash: CAP_DASH };
  }
  return { strokeStyle: GRID_STROKE, lineWidth: 1 };
}

/**
 * The bar fill colour for an interval: a blue -> pink/red ramp by load, where
 * load is `cores/capacity` (or `cores/scaleMax` when capacity is unknown), at
 * 0.42 alpha. Same load value drives the same colour (the coalescer run key,
 * so equal-load neighbours fold into one fillRect).
 */
function cpuBarRgba(
  cores: number,
  capacity: number | null,
  scaleMax: number,
  alpha: number,
): string {
  const load =
    capacity != null
      ? Math.min(1, cores / capacity)
      : Math.min(1, cores / scaleMax);
  const r = Math.round(79 + 176 * load);
  const g = Math.round(195 - 80 * load);
  const b = Math.round(247 - 150 * load);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

export function cpuBarColor(
  cores: number,
  capacity: number | null,
  scaleMax: number,
): string {
  return cpuBarRgba(cores, capacity, scaleMax, 0.42);
}

function cpuBarStrokeColor(
  cores: number,
  capacity: number | null,
  scaleMax: number,
): string {
  return cpuBarRgba(cores, capacity, scaleMax, 0.9);
}

// ── Canvas render ────────────────────────────────────────────────────────

/**
 * Render the CPU track into `ctx` (already DPR-scaled and sized to
 * `geometry.time.drawW` x `geometry.height`) and RETURN the info readout
 * string (the shell mirrors it into the DOM header + a queryable attribute).
 *
 * Draw order: background, then the grid + dashed-capacity strokes via the
 * batched-stroke path (one path per style), then the load-coloured bars via
 * the coalescer (one fillRect per pixel run) and their batched step outline,
 * then the axis text and window markers. Fractional fills + the outline keep
 * sub-pixel loads visible instead of rounding them down to zero height.
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
    // Too short to host a chart: keep the DOM readout, skip the chart body.
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
  if (capacity != null) {
    const capY = chartTop + chartH - (capacity / scaleMax) * chartH;
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
  // parallel runMeta map recovers the colours + top the opaque key stands for.
  const runMeta = new Map<
    string,
    { fill: string; stroke: string; top: number }
  >();
  const barStrokes = makeStrokeBatcher();
  let previousStroke: { right: number; y: number } | null = null;
  const coalescer = makeBarCoalescer((left: number, width: number, key: string) => {
    const meta = runMeta.get(key);
    if (meta === undefined) return;
    const barHeight = baselineY - meta.top;
    if (barHeight > 0) {
      ctx.fillStyle = meta.fill;
      ctx.fillRect(left, meta.top, width, barHeight);
    }
    const y = Math.min(meta.top, baselineY - 0.5);
    const right = left + width;
    const outline = [];
    if (previousStroke !== null && left <= previousStroke.right + 1) {
      outline.push({ x: previousStroke.right, y: previousStroke.y });
      if (left !== previousStroke.right) {
        outline.push({ x: left, y: previousStroke.y });
      }
      if (y !== previousStroke.y) outline.push({ x: left, y });
    } else {
      outline.push({ x: left, y });
    }
    outline.push({ x: right, y });
    barStrokes.polyline(meta.stroke, outline);
    previousStroke = { right, y };
  });
  for (
    let i = firstIntervalEndingAtOrAfter(intervals, viewStart);
    i < intervals.length;
    i++
  ) {
    const interval = intervals[i]!;
    if (interval.start > viewEnd) break;
    const x1 = clampX(nsToDrawX(interval.start, viewStart, viewEnd, drawW), drawW);
    const x2 = clampX(nsToDrawX(interval.end, viewStart, viewEnd, drawW), drawW);
    const fill = cpuBarColor(interval.cores, capacity, scaleMax);
    const stroke = cpuBarStrokeColor(interval.cores, capacity, scaleMax);
    const top = chartTop + chartH - (interval.cores / scaleMax) * chartH;
    const key = `${fill}|${Math.round(top)}`;
    if (!runMeta.has(key)) runMeta.set(key, { fill, stroke, top });
    coalescer.push(x1, x2, key);
  }
  coalescer.flush();
  drawStrokeBatches(ctx, barStrokes.batches(), (strokeStyle) => ({
    strokeStyle,
    lineWidth: 1.5,
  }));

  // ── Text overlay: y-axis scale ──────────────────────────────────
  ctx.fillStyle = Y_LABEL;
  ctx.font = "10px monospace";
  ctx.textAlign = "left";
  // y-axis scale numbers at the draw area's left edge (no in-canvas gutter).
  ctx.fillText(fmtCpuCores(scaleMax), 2, chartTop + 9);
  ctx.fillText("0", 2, baselineY);

  // ── Surface a truncated / oversized window (never as complete) ───
  drawWindowMarkers(ctx, window, drawW, height);

  return readout;
}

/**
 * Surface the resident-window state: a translucent hatch band at each
 * truncated edge (so missing data reads as "not loaded", not "idle CPU"), and
 * a "partial window" badge when a needed segment is oversized. No-ops for a
 * complete window.
 */

// ── Shared draw-area x mapping (the alignment invariant) ──────────────────

/**
 * Timestamp (ns) -> draw-area-relative x (px), identical to the axis track's
 * `nsToDrawX`: NO LABEL_W is added because the track canvas already sits
 * after the DOM label gutter, so this is the same expression every track
 * uses for its canvas-local x (the alignment invariant).
 */

/** Clamp a draw-area x into [0, drawW]. */

export {
  COMPLETE_WINDOW,
  deriveResidentWindow as deriveCpuWindow,
  type ResidentWindow as CpuWindow,
} from "./resident-window.js";
