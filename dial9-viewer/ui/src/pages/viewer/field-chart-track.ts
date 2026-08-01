// Canvas renderer for URL-defined custom-event numeric-field tracks.
//
// Gauge uses a linear stroke. Counter and Up/down counter use filled,
// step-after deltas over the model's explicit intervals; null samples split
// paths, so a monotonic counter reset never invents an increment.
// Downsampling retains first/min/max/last per pixel column, bounding canvas
// work without hiding narrow spikes and without allocating a visible-series
// copy on every pan/zoom.

import { createCanvasSizer, nsToDrawX } from "../../lib/canvas/index.js";
import type { CanvasSizer, Vertex } from "../../lib/canvas/index.js";
import type { PanelGeometry } from "../../types/state.js";
import type {
  FieldChartKind,
  FieldChartSpec,
} from "../../types/state.js";
import type { ViewerStore } from "../../store/store.js";
import { formatFieldValue } from "../../lib/trace/index.js";
import {
  createTooltip,
  tooltipRowsTemplate,
  type TooltipHandle,
  type TooltipRow,
} from "../../components/overlay/tooltip.js";
import {
  createFieldChartSeriesCache,
  type FieldChartNumeric,
  type FieldChartSample,
  type FieldChartSeries,
} from "./field-chart-model.js";

const BG = "#111b2e";
const GRID = "rgba(255,255,255,0.07)";
const LABEL = "#8790a8";
const GAUGE_STROKE = "#55d6be";
const COUNTER_STROKE = "#6ba7ff";
const COUNTER_FILL = "rgba(74, 144, 226, 0.28)";
const UPDOWN_STROKE = "#d19cff";
const UPDOWN_POSITIVE = "rgba(85, 214, 190, 0.25)";
const UPDOWN_NEGATIVE = "rgba(255, 107, 129, 0.25)";
const ZERO_STROKE = "rgba(255,255,255,0.24)";
const RESET_STROKE = "rgba(255, 184, 77, 0.72)";
const CHART_TOP = 20;
const CHART_BOTTOM_PAD = 8;
const BIGINT_RATIO_SCALE = 1_000_000_000_000n;
const MAX_SAFE_BIGINT = BigInt(Number.MAX_SAFE_INTEGER);

export interface FieldChartPlot {
  segments: readonly (readonly Vertex[])[];
  resetXs: readonly number[];
  min: FieldChartNumeric | null;
  max: FieldChartNumeric | null;
  baselineY: number;
  stats: FieldChartStats;
}

export interface FieldChartStats {
  count: number;
  sum: FieldChartNumeric | null;
  min: FieldChartNumeric | null;
  max: FieldChartNumeric | null;
}

interface RawPoint {
  index: number;
  timestamp: number;
  value: FieldChartNumeric;
}

interface Bucket {
  column: number;
  first: RawPoint;
  min: RawPoint;
  max: RawPoint;
  last: RawPoint;
}

function compare(a: FieldChartNumeric, b: FieldChartNumeric): number {
  if (typeof a === "bigint" && typeof b === "bigint") {
    return a < b ? -1 : a > b ? 1 : 0;
  }
  const an = Number(a);
  const bn = Number(b);
  return an < bn ? -1 : an > bn ? 1 : 0;
}

function zeroFor(value: FieldChartNumeric): FieldChartNumeric {
  return typeof value === "bigint" ? 0n : 0;
}

function add(
  a: FieldChartNumeric,
  b: FieldChartNumeric,
): FieldChartNumeric | null {
  if (typeof a === "bigint" && typeof b === "bigint") return a + b;
  const sum = Number(a) + Number(b);
  return Number.isFinite(sum) ? sum : null;
}

function emptyStats(): FieldChartStats {
  return { count: 0, sum: null, min: null, max: null };
}

function includeStat(stats: FieldChartStats, value: FieldChartNumeric): void {
  stats.sum =
    stats.count === 0
      ? value
      : stats.sum === null
        ? null
        : add(stats.sum, value);
  stats.count++;
  if (stats.min === null || compare(value, stats.min) < 0) stats.min = value;
  if (stats.max === null || compare(value, stats.max) > 0) stats.max = value;
}

function ratio(
  value: FieldChartNumeric,
  min: FieldChartNumeric,
  max: FieldChartNumeric,
): number {
  if (compare(min, max) === 0) return 0.5;
  if (
    typeof value === "bigint" &&
    typeof min === "bigint" &&
    typeof max === "bigint"
  ) {
    return Number(
      ((value - min) * BIGINT_RATIO_SCALE) / (max - min),
    ) / Number(BIGINT_RATIO_SCALE);
  }
  const v = Number(value);
  const lo = Number(min);
  const hi = Number(max);
  // Divide before subtracting so [-1e308, 1e308] does not overflow.
  const scale = Math.max(Math.abs(lo), Math.abs(hi), Math.abs(v), 1);
  const normalized = (v / scale - lo / scale) / (hi / scale - lo / scale);
  if (Number.isFinite(normalized)) {
    return Math.min(1, Math.max(0, normalized));
  }
  // Mixed decimal/BigInt series can exceed Number's range. Keep the canvas
  // coordinates finite even when an exact mixed-type ratio is unavailable.
  if (compare(value, min) <= 0) return 0;
  if (compare(value, max) >= 0) return 1;
  return 0.5;
}

function lowerBound(
  samples: readonly FieldChartSample[],
  timestamp: number,
): number {
  let lo = 0;
  let hi = samples.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (samples[mid]!.timestamp < timestamp) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

function upperBound(
  samples: readonly FieldChartSample[],
  timestamp: number,
): number {
  let lo = 0;
  let hi = samples.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (samples[mid]!.timestamp <= timestamp) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

export interface FieldChartHover {
  value: FieldChartNumeric;
}

/**
 * Resolve the value under a timestamp without scanning the series. Gauge only
 * considers the two adjacent entries, so it never skips over a run of explicit
 * gaps. Counters resolve the exact half-open interval.
 */
export function fieldChartHoverAt(
  series: FieldChartSeries,
  kind: FieldChartKind,
  timestamp: number,
): FieldChartHover | null {
  const { samples } = series;
  if (kind !== "gauge") {
    const sample = samples[upperBound(samples, timestamp) - 1];
    return sample !== undefined &&
      sample.value !== null &&
      sample.endTimestamp !== null &&
      timestamp < sample.endTimestamp
      ? {
          value: sample.value,
        }
      : null;
  }

  const index = lowerBound(samples, timestamp);
  const before = samples[index - 1];
  const after = samples[index];
  if (
    after !== undefined &&
    after.timestamp === timestamp &&
    after.value === null
  ) {
    return null;
  }
  const beforeNumeric =
    before !== undefined &&
    before.value !== null &&
    before.endTimestamp === null
      ? before
      : null;
  const afterNumeric =
    after !== undefined &&
    after.value !== null &&
    after.endTimestamp === null
      ? after
      : null;
  const sample =
    beforeNumeric === null
      ? afterNumeric
      : afterNumeric === null ||
          timestamp - beforeNumeric.timestamp <=
            afterNumeric.timestamp - timestamp
        ? beforeNumeric
        : afterNumeric;
  if (sample === null || sample.value === null) return null;
  return { value: sample.value };
}

/** Structured content for the shared viewer tooltip. */
export function fieldChartTooltipRows(
  hover: FieldChartHover,
  spec: FieldChartSpec,
  unit: string | null,
): TooltipRow[] {
  return [
    [
      {
        label: `${spec.fieldName}:`,
        value: fieldChartTooltipValue(hover.value, unit),
      },
    ],
  ];
}

function pixelColumn(x: number, drawW: number): number {
  return Math.min(
    Math.max(0, Math.floor(x)),
    Math.max(0, Math.ceil(drawW) - 1),
  );
}

function bucketPoints(bucket: Bucket): RawPoint[] {
  return [
    bucket.first,
    bucket.min,
    bucket.max,
    bucket.last,
  ]
    .sort((a, b) => a.index - b.index)
    .filter((point, i, all) => i === 0 || point.index !== all[i - 1]!.index);
}

function stepAfter(vertices: readonly Vertex[]): Vertex[] {
  if (vertices.length < 2) return [...vertices];
  const out: Vertex[] = [vertices[0]!];
  for (let i = 1; i < vertices.length; i++) {
    const previous = vertices[i - 1]!;
    const next = vertices[i]!;
    out.push({ x: next.x, y: previous.y }, next);
  }
  return out;
}

/**
 * Build a pixel-bounded plot for the visible window. Gauge keeps one neighboring
 * observation on either side so the line intersects the viewport correctly.
 * Counter samples already describe explicit intervals and are clipped to their
 * own end, never carried through an unobserved span. Nulls split segments.
 */
export function buildFieldChartPlot(
  series: FieldChartSeries,
  kind: FieldChartKind,
  viewStart: number,
  viewEnd: number,
  drawW: number,
  chartTop: number,
  chartBottom: number,
): FieldChartPlot {
  const { samples } = series;
  if (samples.length === 0 || drawW <= 0 || viewEnd <= viewStart) {
    return {
      segments: [],
      resetXs: [],
      min: null,
      max: null,
      baselineY: chartBottom,
      stats: emptyStats(),
    };
  }

  const from = Math.max(0, lowerBound(samples, viewStart) - 1);
  const to = Math.min(
    samples.length,
    upperBound(samples, viewEnd) + (kind === "gauge" ? 1 : 0),
  );
  let min: FieldChartNumeric | null = null;
  let max: FieldChartNumeric | null = null;
  const stats = emptyStats();
  for (let i = from; i < to; i++) {
    const sample = samples[i]!;
    if (sample.value === null) continue;
    const visible =
      kind === "gauge"
        ? sample.timestamp >= viewStart && sample.timestamp <= viewEnd
        : sample.endTimestamp !== null &&
          sample.endTimestamp > viewStart &&
          sample.timestamp < viewEnd;
    if (visible) includeStat(stats, sample.value);
    if (kind !== "gauge" && !visible) continue;
    if (min === null || compare(sample.value, min) < 0) min = sample.value;
    if (max === null || compare(sample.value, max) > 0) max = sample.value;
  }
  if (min === null || max === null) {
    return {
      segments: [],
      resetXs: [],
      min: null,
      max: null,
      baselineY: chartBottom,
      stats,
    };
  }

  if (kind !== "gauge") {
    const zero = zeroFor(min);
    if (compare(zero, min) < 0) min = zero;
    if (compare(zero, max) > 0) max = zero;
  }
  const scaleMin = min;
  const scaleMax = max;
  const flatZeroCounter =
    kind === "counter" && compare(scaleMin, scaleMax) === 0;
  const chartH = Math.max(1, chartBottom - chartTop);
  const yAt = (value: FieldChartNumeric): number =>
    flatZeroCounter
      ? chartBottom
      : chartBottom - ratio(value, scaleMin, scaleMax) * chartH;
  const baselineY =
    kind === "gauge" ? chartBottom : yAt(zeroFor(scaleMin));

  const segments: Vertex[][] = [];
  const resetXs = new Set<number>();
  let rawSegment: RawPoint[] = [];
  let bucket: Bucket | null = null;
  let segmentEnd: number | null = null;

  const flushBucket = (): void => {
    if (bucket === null) return;
    rawSegment.push(...bucketPoints(bucket));
    bucket = null;
  };

  const flushSegment = (): void => {
    flushBucket();
    if (rawSegment.length === 0) return;
    let vertices = rawSegment.map((point) => ({
      x: nsToDrawX(point.timestamp, viewStart, viewEnd, drawW),
      y: yAt(point.value),
    }));
    if (kind !== "gauge") {
      vertices = stepAfter(vertices);
      if (segmentEnd !== null) {
        const x = nsToDrawX(segmentEnd, viewStart, viewEnd, drawW);
        const last = vertices[vertices.length - 1]!;
        if (x > last.x) vertices.push({ x, y: last.y });
      }
    }
    segments.push(vertices);
    rawSegment = [];
    segmentEnd = null;
  };

  for (let i = from; i < to; i++) {
    const sample = samples[i]!;
    if (sample.value === null) {
      flushSegment();
      if (
        sample.gap === "reset" &&
        sample.timestamp >= viewStart &&
        sample.timestamp <= viewEnd
      ) {
        resetXs.add(
          pixelColumn(
            nsToDrawX(sample.timestamp, viewStart, viewEnd, drawW),
            drawW,
          ),
        );
      }
      continue;
    }
    if (
      kind !== "gauge" &&
      (sample.endTimestamp === null ||
        sample.endTimestamp <= viewStart ||
        sample.timestamp >= viewEnd)
    ) {
      continue;
    }
    if (
      kind !== "gauge" &&
      segmentEnd !== null &&
      sample.timestamp !== segmentEnd
    ) {
      flushSegment();
    }
    const point: RawPoint = {
      index: i,
      timestamp: sample.timestamp,
      value: sample.value,
    };
    const x = nsToDrawX(sample.timestamp, viewStart, viewEnd, drawW);
    const column = pixelColumn(x, drawW);
    if (bucket === null || bucket.column !== column) {
      flushBucket();
      bucket = {
        column,
        first: point,
        min: point,
        max: point,
        last: point,
      };
    } else {
      bucket.last = point;
      if (compare(point.value, bucket.min.value) < 0) bucket.min = point;
      if (compare(point.value, bucket.max.value) > 0) bucket.max = point;
    }
    segmentEnd = sample.endTimestamp;
  }
  flushSegment();

  return {
    segments,
    resetXs: [...resetXs],
    min,
    max,
    baselineY,
    stats,
  };
}

function path(
  ctx: CanvasRenderingContext2D,
  vertices: readonly Vertex[],
): void {
  const first = vertices[0];
  if (first === undefined) return;
  ctx.moveTo(first.x, first.y);
  for (let i = 1; i < vertices.length; i++) {
    const vertex = vertices[i]!;
    ctx.lineTo(vertex.x, vertex.y);
  }
}

function fillSegments(
  ctx: CanvasRenderingContext2D,
  segments: readonly (readonly Vertex[])[],
  baselineY: number,
  fillStyle: string,
): void {
  ctx.fillStyle = fillStyle;
  for (const vertices of segments) {
    const first = vertices[0];
    const last = vertices[vertices.length - 1];
    if (first === undefined || last === undefined) continue;
    ctx.beginPath();
    ctx.moveTo(first.x, baselineY);
    ctx.lineTo(first.x, first.y);
    for (let i = 1; i < vertices.length; i++) {
      const vertex = vertices[i]!;
      ctx.lineTo(vertex.x, vertex.y);
    }
    ctx.lineTo(last.x, baselineY);
    ctx.closePath();
    ctx.fill();
  }
}

function strokeSegments(
  ctx: CanvasRenderingContext2D,
  segments: readonly (readonly Vertex[])[],
  color: string,
): void {
  ctx.beginPath();
  for (const vertices of segments) path(ctx, vertices);
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.5;
  ctx.lineJoin = "round";
  ctx.stroke();
}

function compactBigInt(value: bigint): string {
  const text = String(value);
  const sign = text.startsWith("-") ? "-" : "";
  const digits = sign === "" ? text : text.slice(1);
  return digits.length <= 18
    ? text
    : `${sign}${digits[0]}.${digits.slice(1, 4)}e${digits.length - 1}`;
}

function unitAwareFieldChartValue(
  value: FieldChartNumeric,
  unit: string | null,
  raw: string,
): string {
  if (unit === null) return raw;
  if (!["ns", "us", "ms", "s", "bytes"].includes(unit)) {
    return `${raw} ${unit}`;
  }
  const safeBigInt =
    typeof value !== "bigint" ||
    (value >= -MAX_SAFE_BIGINT && value <= MAX_SAFE_BIGINT);
  const n = Number(value);
  const multiplier =
    unit === "us"
      ? 1e3
      : unit === "ms"
        ? 1e6
        : unit === "s"
          ? 1e9
          : 1;
  return safeBigInt && n >= 0 && Number.isFinite(n * multiplier)
    ? formatFieldValue(value, unit)
    : `${raw} ${unit}`;
}

function fieldChartTooltipValue(
  value: FieldChartNumeric,
  unit: string | null,
): string {
  return unitAwareFieldChartValue(value, unit, String(value));
}

export function compactFieldChartValue(
  value: FieldChartNumeric,
  unit: string | null,
): string {
  const raw = typeof value === "bigint" ? compactBigInt(value) : String(value);
  if (unit !== null) return unitAwareFieldChartValue(value, unit, raw);
  if (typeof value === "bigint") return raw;
  if (value === 0) return "0";
  const abs = Math.abs(value);
  if (abs >= 1e9 || abs < 0.001) return value.toExponential(3);
  return String(Number(value.toPrecision(5)));
}

function clippedLabel(value: FieldChartNumeric, unit: string | null): string {
  const text = compactFieldChartValue(value, unit);
  return text.length <= 22 ? text : `${text.slice(0, 21)}\u2026`;
}

function averageValue(stats: FieldChartStats): FieldChartNumeric | null {
  if (stats.count === 0 || stats.sum === null) return null;
  if (typeof stats.sum === "number") return stats.sum / stats.count;
  const divisor = BigInt(stats.count);
  const quotient = stats.sum / divisor;
  const remainder = stats.sum % divisor;
  if (remainder === 0n) return quotient;
  if (stats.sum >= -MAX_SAFE_BIGINT && stats.sum <= MAX_SAFE_BIGINT) {
    return Number(stats.sum) / stats.count;
  }
  const roundsAwayFromZero =
    (remainder < 0n ? -remainder : remainder) * 2n >= divisor;
  return roundsAwayFromZero
    ? quotient + (stats.sum < 0n ? -1n : 1n)
    : quotient;
}

/** Compact visible-window summary shown in the track's top-right corner. */
export function fieldChartReadoutText(
  stats: FieldChartStats,
  unit: string | null,
  kind: FieldChartKind,
): string {
  const average = averageValue(stats);
  if (average === null || stats.max === null) return "";
  const parts = [
    `avg ${compactFieldChartValue(average, unit)}`,
  ];
  if (kind === "updown-counter" && stats.min !== null) {
    parts.push(`min ${compactFieldChartValue(stats.min, unit)}`);
  }
  parts.push(`max ${compactFieldChartValue(stats.max, unit)}`);
  return parts.join(" \u00b7 ");
}

export function renderFieldChart(
  ctx: CanvasRenderingContext2D,
  geometry: PanelGeometry,
  series: FieldChartSeries,
  spec: FieldChartSpec,
  viewStart: number,
  viewEnd: number,
): void {
  const drawW = geometry.time.drawW;
  const height = geometry.height;
  ctx.clearRect(0, 0, drawW, height);
  ctx.fillStyle = BG;
  ctx.fillRect(0, 0, drawW, height);
  if (drawW <= 0 || viewEnd <= viewStart) return;

  const chartBottom = Math.max(CHART_TOP + 1, height - CHART_BOTTOM_PAD);
  const plot = buildFieldChartPlot(
    series,
    spec.kind,
    viewStart,
    viewEnd,
    drawW,
    CHART_TOP,
    chartBottom,
  );

  ctx.strokeStyle = GRID;
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let i = 1; i <= 3; i++) {
    const y = CHART_TOP + ((chartBottom - CHART_TOP) * i) / 4;
    ctx.moveTo(0, y);
    ctx.lineTo(drawW, y);
  }
  ctx.stroke();

  if (spec.kind === "updown-counter") {
    ctx.strokeStyle = ZERO_STROKE;
    ctx.beginPath();
    ctx.moveTo(0, plot.baselineY);
    ctx.lineTo(drawW, plot.baselineY);
    ctx.stroke();
  }

  if (plot.segments.length > 0) {
    if (spec.kind === "counter") {
      fillSegments(ctx, plot.segments, plot.baselineY, COUNTER_FILL);
      strokeSegments(ctx, plot.segments, COUNTER_STROKE);
    } else if (spec.kind === "updown-counter") {
      ctx.save();
      ctx.beginPath();
      ctx.rect(0, CHART_TOP, drawW, Math.max(0, plot.baselineY - CHART_TOP));
      ctx.clip();
      fillSegments(ctx, plot.segments, plot.baselineY, UPDOWN_POSITIVE);
      ctx.restore();
      ctx.save();
      ctx.beginPath();
      ctx.rect(0, plot.baselineY, drawW, Math.max(0, chartBottom - plot.baselineY));
      ctx.clip();
      fillSegments(ctx, plot.segments, plot.baselineY, UPDOWN_NEGATIVE);
      ctx.restore();
      strokeSegments(ctx, plot.segments, UPDOWN_STROKE);
    } else {
      strokeSegments(ctx, plot.segments, GAUGE_STROKE);
      for (const vertices of plot.segments) {
        if (vertices.length !== 1) continue;
        const point = vertices[0]!;
        ctx.fillStyle = GAUGE_STROKE;
        ctx.beginPath();
        ctx.arc(point.x, point.y, 2, 0, Math.PI * 2);
        ctx.fill();
      }
    }
  }

  if (plot.resetXs.length > 0) {
    ctx.save();
    ctx.strokeStyle = RESET_STROKE;
    ctx.setLineDash([3, 3]);
    ctx.beginPath();
    for (const x of plot.resetXs) {
      ctx.moveTo(x + 0.5, CHART_TOP);
      ctx.lineTo(x + 0.5, chartBottom);
    }
    ctx.stroke();
    ctx.restore();
  }

  ctx.fillStyle = LABEL;
  ctx.font = "10px monospace";
  ctx.textAlign = "left";
  if (plot.max !== null) {
    ctx.fillText(clippedLabel(plot.max, series.unit), 3, CHART_TOP - 7);
  }
  if (plot.min !== null) {
    ctx.fillText(clippedLabel(plot.min, series.unit), 3, chartBottom);
  }
  ctx.textAlign = "right";
  const readout = fieldChartReadoutText(plot.stats, series.unit, spec.kind);
  if (readout !== "") {
    ctx.fillText(readout, Math.max(0, drawW - 5), 11);
  }
  if (plot.segments.length === 0) {
    ctx.textAlign = "center";
    ctx.fillText(
      spec.kind === "gauge" ? "No numeric samples" : "No numeric intervals",
      drawW / 2,
      height / 2,
    );
  }
}

export interface FieldChartTrackController {
  reconcile(specs: readonly FieldChartSpec[]): void;
  paint(
    canvas: HTMLCanvasElement,
    geometry: PanelGeometry,
    spec: FieldChartSpec,
    viewStart: number,
    viewEnd: number,
  ): void;
  dispose(): void;
}

interface FieldChartPaintState {
  spec: FieldChartSpec;
  viewStart: number;
  viewEnd: number;
  drawW: number;
}

interface FieldChartCanvasBinding {
  canvas: HTMLCanvasElement;
  state: FieldChartPaintState;
  onMouseMove: (event: MouseEvent) => void;
  onMouseLeave: () => void;
}

/** Create the one controller/cache shared by all dynamic tracks in this view. */
export function createFieldChartTrack(
  store: ViewerStore,
): FieldChartTrackController {
  const cache = createFieldChartSeriesCache();
  const sizers = new WeakMap<
    HTMLCanvasElement,
    CanvasSizer<CanvasRenderingContext2D>
  >();
  const bindings = new Map<string, FieldChartCanvasBinding>();
  let tooltip: TooltipHandle | null = null;

  function removeBinding(id: string): void {
    const binding = bindings.get(id);
    if (binding === undefined) return;
    binding.canvas.removeEventListener("mousemove", binding.onMouseMove);
    binding.canvas.removeEventListener("mouseleave", binding.onMouseLeave);
    bindings.delete(id);
    tooltip?.hide();
  }

  function bindCanvas(
    canvas: HTMLCanvasElement,
    state: FieldChartPaintState,
  ): void {
    const existing = bindings.get(state.spec.id);
    if (existing?.canvas === canvas) {
      existing.state = state;
      return;
    }
    if (existing !== undefined) removeBinding(state.spec.id);

    const id = state.spec.id;
    const onMouseMove = (event: MouseEvent): void => {
      const binding = bindings.get(id);
      if (binding === undefined || binding.canvas !== canvas) return;
      const current = binding.state;
      const trace = store.getState().trace.trace;
      if (trace === null) {
        tooltip?.hide();
        return;
      }
      const series = cache.get(trace, current.spec);
      const x = Math.min(Math.max(0, event.offsetX), current.drawW);
      const timestamp =
        current.viewStart +
        (x / (current.drawW || 1)) *
          (current.viewEnd - current.viewStart);
      const hover = fieldChartHoverAt(
        series,
        current.spec.kind,
        timestamp,
      );
      if (hover === null) {
        tooltip?.hide();
        return;
      }

      tooltip ??= createTooltip(canvas.ownerDocument);
      tooltip.show(
        tooltipRowsTemplate(
          fieldChartTooltipRows(
            hover,
            current.spec,
            series.unit,
          ),
        ),
        event,
      );
    };
    const onMouseLeave = (): void => tooltip?.hide();
    canvas.addEventListener("mousemove", onMouseMove);
    canvas.addEventListener("mouseleave", onMouseLeave);
    bindings.set(id, {
      canvas,
      state,
      onMouseMove,
      onMouseLeave,
    });
  }

  return {
    reconcile(specs) {
      const trace = store.getState().trace.trace;
      cache.reconcile(trace, specs);
      const live = new Set(
        trace === null ? [] : specs.map((spec) => spec.id),
      );
      for (const id of bindings.keys()) {
        if (!live.has(id)) removeBinding(id);
      }
      if (bindings.size === 0 && tooltip !== null) {
        tooltip.dispose();
        tooltip = null;
      }
    },
    paint(canvas, geometry, spec, viewStart, viewEnd) {
      const trace = store.getState().trace.trace;
      if (trace === null) {
        removeBinding(spec.id);
        return;
      }
      let sizer = sizers.get(canvas);
      if (sizer === undefined) {
        sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
        sizers.set(canvas, sizer);
      }
      const ctx = sizer.ensure(
        geometry.time.drawW,
        geometry.height,
        geometry.dpr,
      );
      const series = cache.get(trace, spec);
      bindCanvas(canvas, {
        spec,
        viewStart,
        viewEnd,
        drawW: geometry.time.drawW,
      });
      renderFieldChart(
        ctx,
        geometry,
        series,
        spec,
        viewStart,
        viewEnd,
      );
    },
    dispose() {
      for (const id of [...bindings.keys()]) removeBinding(id);
      tooltip?.dispose();
      tooltip = null;
      cache.clear();
    },
  };
}
