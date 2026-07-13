// src/pages/viewer/cpu.ts - the Process CPU usage track (T28;
// docs/ui-inventory/features/02-viewer-html.md section L; legacy
// `renderProcessCpuPanel`/`visibleCpuStats`/`fmtCpuCores`/`cpuIntervalTooltipHtml`
// in viewer.html).
//
// The CPU track fills the unified column's "cpu" slot (track-layout.ts,
// TRACKS "cpu"). Rendering reality (features/02 L1): it is a fillRect BAR
// chart of avg-cores-over-time, NOT a curve. The bars go through the T08
// run-length coalescer (lib/canvas makeBarCoalescer); the ONLY stroked
// primitives - the 25/50/75% grid lines and the dashed capacity line - go
// through the T08 batched-stroke path (drawStrokeBatches, one path per
// style: 03 F1's rule). The two paths are never mixed.
//
// LAYOUT SEAM vs legacy (same as the axis, axis.ts): the legacy
// `#cpu-panel-canvas` spanned the FULL panel width and offset every x by
// LABEL_W inline (`nsToPanelXClamped` = `LABEL_W + nsToX`). In the new shell
// the track canvas is `drawW` wide and sits AFTER a LABEL_W DOM label gutter
// (tracks.ts), so the chart draws in DRAW-AREA-relative coordinates
// (`nsToDrawX` = the legacy mapping with NO LABEL_W added). The DOM gutter
// supplies the same offset for every track, so the CPU bars line up
// pixel-exact with the lanes/axis by construction (the A13 alignment
// invariant). The y-axis scale numbers, which the legacy drew in the LABEL_W
// gutter (`x = LABEL_W - 6`), now draw at the draw area's left edge (there
// is no in-canvas gutter to hold them) - the same gutter->draw-area shift
// the axis track made for the identical reason.
//
// READOUT (features/02 L2): the legacy top-right info span
// (`avg X cores [· avg Y%] · max Z`) is ported EXACTLY -
// `cpuReadoutText` reproduces it to the digit (the DoD's behavioral-diff
// target). `renderCpuTrack` returns it so the shell can mirror it into a
// DOM-queryable attribute (tracks.ts) and also paints it on the canvas.
//
// HOVER (features/02 L3/L4): the tooltip RENDERING + crosshair live in the
// T24 overlay/tooltip system (sections I + V; not a T28 dep, not yet
// landed). T28 owns the CPU-specific CONTENT: `cpuIntervalAt` (the
// binary-search lookup, legacy `findProcessCpuIntervalAt`) and
// `cpuIntervalTooltip` (the label/value lines, legacy
// `cpuIntervalTooltipHtml`). T24 wires them; until then their rendered
// behavior is deferred (see HANDOFF).
//
// WINDOWING (T17 carried obligation, binding on every track renderer): when
// the CPU data is segment-windowed, the renderer MUST NOT paint a truncated
// window as if it were complete. `renderCpuTrack` consumes a `CpuWindow`
// descriptor and surfaces truncation (an edge hatch band) and the
// `oversized` segment state (a "partial window" badge) rather than drawing a
// clean edge. The viewer shell currently loads WHOLE traces (the segments
// slice is empty), so `deriveCpuInputs` resolves the descriptor to
// "complete"; the derivation from a live resident window is the downstream
// wiring seam (T34/T35). The renderer's consumption of the descriptor is
// exercised directly by Vitest.

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

// ── Windowing descriptor (T17 obligation) ───────────────────────────────

/**
 * The resident-data-window state the CPU renderer must surface so a
 * truncated/partial window is never painted as complete (T17-audit contract
 * notes 6+7, carried obligation). Both fields resolve to the "complete"
 * value ({ truncatedAt: null, oversized: false }) for a whole-trace load.
 */
export interface CpuWindow {
  /**
   * Which edge(s) of the resident window truncate the CPU data (mirrors the
   * `WindowEdgePoll.truncatedAt` union, types/trace.d.ts): data beyond the
   * edge is not resident, so that edge is a WINDOW boundary, not the true
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
   * The avg-cores-over-time intervals (frozen-core
   * `buildProcessCpuUsageSeries`), sorted ascending by `start` and
   * contiguous except where a sample was dropped. Memoized on the trace
   * identity - see `cpuSeriesFor`.
   */
  intervals: readonly ProcessCpuUsageInterval[];
  /**
   * Available parallelism = the CPU "capacity" (features/02 L1/L5), the
   * normalized `process.available_parallelism` segment metadatum; null when
   * unknown (no capacity line, no percent readout).
   */
  capacity: number | null;
  /** Resident-window truncation state (T17 obligation). */
  window: CpuWindow;
}

// buildProcessCpuUsageSeries scans every custom event, so memoize on the
// ParsedTrace identity: the trace slice is replaced wholesale on load
// (store contract), so a new trace object is a real cache miss and a stable
// one (pan/zoom/selection frames) is a hit. Mirrors the `sizers` WeakMap in
// tracks.ts.
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
 * Derive the CPU window descriptor from the store (T17 obligation). The
 * `oversized` state is surfaced directly from the segments slice - any
 * needed segment stuck in "oversized" makes the view partial. `truncatedAt`
 * derivation from the resident window (computeWindowBoundaryPolls) is the
 * downstream wiring seam (T34/T35 feed windowed data into the viewer); the
 * whole-trace shell has an empty segments slice, so it resolves to null
 * here while the renderer consumes it regardless (tested).
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

// ── Visible-window stats (features/02 L2; legacy visibleCpuStats) ────────

export interface CpuStats {
  /** Time-weighted mean cores over the visible window (0 when no overlap). */
  avgCores: number;
  /** Peak interval cores touching the visible window. */
  maxCores: number;
}

/**
 * The avg/max readout stats for the visible window - ported VERBATIM from
 * the legacy `visibleCpuStats` (viewer.html:3653-3668): `avgCores` is the
 * overlap-weighted mean of `interval.cores`, `maxCores` the peak interval
 * touching the window. Intervals are sorted by start, so it breaks once
 * past the right edge. This is the DoD's exact behavioral-diff target.
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

// ── Formatting (features/02 L2; legacy fmtCpuCores/fmtCpuPercent) ────────

/**
 * Core count -> compact label (legacy `fmtCpuCores`, viewer.html:3640-3646):
 * 1 decimal at >=10 cores else 2, trailing zeros and a bare dot stripped;
 * non-finite renders `-`. Ported verbatim (behavioral-diff exact).
 */
export function fmtCpuCores(value: number): string {
  if (!Number.isFinite(value)) return "-";
  return value
    .toFixed(value >= 10 ? 1 : 2)
    .replace(/(\.\d*?)0+$/, "$1")
    .replace(/\.$/, "");
}

/**
 * Percent -> `NN.N%` (legacy `fmtCpuPercent`, viewer.html:3648-3651);
 * non-finite renders `-`. Ported verbatim.
 */
export function fmtCpuPercent(value: number): string {
  if (!Number.isFinite(value)) return "-";
  return `${value.toFixed(1)}%`;
}

// The legacy info label joins its parts with a middle dot (U+00B7). Kept as
// an escape so this source stays plain ASCII while the OUTPUT matches the
// legacy string byte-for-byte (the behavioral-diff requirement, L2).
const SEP = " · ";

/**
 * The visible-window info readout (features/02 L2), ported EXACTLY from
 * viewer.html:3677-3680: `avg <cores> cores`, then (only when capacity is
 * known) `· avg <pct>%` where pct = min(100, avg/capacity*100), then
 * `· max <cores>`. This string must match the legacy `#cpu-panel-info`
 * to the digit.
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

// ── Hover content (features/02 L3; the T24 seam) ─────────────────────────

/**
 * The interval under a timestamp, or null (legacy `findProcessCpuIntervalAt`,
 * viewer.html:3750-3761): binary search over the sorted, non-overlapping
 * intervals. This is the CPU track's half of the L3 hover seam - T24's
 * tooltip/crosshair system consumes it; T28 does not render the tooltip.
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
 * The CPU hover tooltip content (features/02 L3), ported from the legacy
 * `cpuIntervalTooltipHtml` (viewer.html:3763-3772) as structured rows so
 * T24 renders them without T28 touching the DOM: Window / CPU time / Cores,
 * plus Total CPU % only when capacity is known.
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

// ── Colors + geometry (features/02 L1; legacy renderProcessCpuPanel) ─────

// Legacy CPU panel colours (viewer.html), kept identical so the migrated
// track reads the same.
const CPU_BG = "#111b2e";
const Y_LABEL = "#667";
const GRID_STROKE = "rgba(255,255,255,0.07)";
const CAP_STROKE = "rgba(255,207,153,0.65)";
const CAP_LABEL = "#ffcf99";
const READOUT_FILL = "#aaa";
const TRUNC_BAND = "rgba(255,120,120,0.28)";
const TRUNC_LABEL = "#ffb3b3";
const CAP_DASH: readonly number[] = [4, 3];

// Vertical chart margins (legacy: chartTop=20 headroom for labels, chartH =
// ph-28, i.e. 8px bottom margin). Taken relative to the track height from
// layout, never the retired 92px fold height.
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
 * The bar fill colour for an interval, ported verbatim from
 * viewer.html:3733-3739: a blue -> pink/red ramp by load, where load is
 * `cores/capacity` (or `cores/scaleMax` when capacity is unknown), at 0.42
 * alpha. Same load value drives the same colour (used as the coalescer run
 * key, so equal-load neighbours fold into one fillRect).
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

// ── Canvas render (features/02 L1/L2) ────────────────────────────────────

/**
 * Render the CPU track into `ctx` (already DPR-scaled and sized to
 * `geometry.time.drawW` x `geometry.height`) and RETURN the L2 info readout
 * string (the shell mirrors it into a DOM-queryable attribute, tracks.ts).
 *
 * Draw order mirrors the legacy `renderProcessCpuPanel`: background, then the
 * grid + dashed-capacity strokes via the T08 BATCHED-STROKE path (one path
 * per style, F1), then the load-coloured bars via the T08 COALESCER (one
 * fillRect per pixel run), then the axis/capacity/readout text on top, then
 * the T17 window markers (most prominent). The two render paths are never
 * mixed.
 *
 * Called from tracks.ts `sizeTracks` for the "cpu" track, inside the store's
 * frame tick (the one place renders run + layout reads batch, F3). A blank
 * background is painted before the trace loads / when the panel is too
 * narrow so the slot still reads as a track; the readout is returned even
 * then (empty-window stats) so the shell stays consistent.
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
    // Too short to host a chart (e.g. a future collapsed track): keep the
    // readout, skip the chart body.
    paintReadout(ctx, readout, drawW);
    return readout;
  }

  // Scale: 0 .. max(1, visible max cores, capacity) - legacy line 3691.
  const scaleMax = Math.max(1, stats.maxCores, capacity ?? 0);

  // ── Strokes: 25/50/75% grid + dashed capacity, batched by style (F1) ──
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
  // Key = fill colour + pixel-rounded top, so contiguous bars of equal
  // height AND colour (the flat-CPU common case) fold into one fillRect,
  // while a taller/shorter or differently-loaded neighbour starts a new run
  // (no height is lost - see the module header). The parallel runMeta map
  // recovers the colour + top the coalescer's opaque key stands for.
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
  // y-axis scale numbers: legacy drew these right-aligned in the LABEL_W
  // gutter; the new track has no in-canvas gutter, so they sit at the draw
  // area's left edge (same gutter->draw-area shift as the axis track).
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

  // ── T17: surface a truncated / oversized window (never as complete) ───
  drawWindowMarkers(ctx, window, drawW, height);

  return readout;
}

/** Draw the L2 info readout at the draw area's top-right (legacy position). */
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
 * Surface the resident-window state (T17 obligation): a translucent hatch
 * band at each truncated edge (so the missing data reads as "not loaded",
 * not "idle CPU"), and a "partial window" badge when a needed segment is
 * oversized. No-ops for a complete window - nothing extra is drawn.
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

// ── Shared draw-area x mapping (the A13 alignment invariant) ──────────────

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

/** Clamp a draw-area x into [0, drawW] (legacy `nsToPanelXClamped`). */
function clampX(x: number, drawW: number): number {
  return Math.min(Math.max(x, 0), drawW);
}
