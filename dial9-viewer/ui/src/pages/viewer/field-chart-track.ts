// Canvas renderer for URL-defined custom-event numeric-field tracks.
//
// Gauge uses a linear stroke. Counter and Up/down counter use step-after
// strokes with filled areas; the model's null samples split paths, so a
// monotonic counter reset never draws a misleading vertical connection.
// Downsampling retains first/min/max/last per pixel column, bounding canvas
// work without hiding narrow spikes and without allocating a visible-series
// copy on every pan/zoom.

import { createCanvasSizer } from "../../lib/canvas/dpr.js";
import type { CanvasSizer } from "../../lib/canvas/dpr.js";
import type { PanelGeometry } from "../../types/state.js";
import type {
  FieldChartKind,
  FieldChartSpec,
} from "../../types/state.js";
import type { ViewerStore } from "../../store/store.js";
import { formatFieldValue } from "../../lib/trace/index.js";
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

export interface FieldChartVertex {
  x: number;
  y: number;
}

export interface FieldChartPlot {
  segments: readonly (readonly FieldChartVertex[])[];
  resetXs: readonly number[];
  min: FieldChartNumeric | null;
  max: FieldChartNumeric | null;
  baselineY: number;
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
  return (v / scale - lo / scale) / (hi / scale - lo / scale);
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

function xAt(
  timestamp: number,
  viewStart: number,
  viewEnd: number,
  drawW: number,
): number {
  return ((timestamp - viewStart) / (viewEnd - viewStart || 1)) * drawW;
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

function stepAfter(vertices: readonly FieldChartVertex[]): FieldChartVertex[] {
  if (vertices.length < 2) return [...vertices];
  const out: FieldChartVertex[] = [vertices[0]!];
  for (let i = 1; i < vertices.length; i++) {
    const previous = vertices[i - 1]!;
    const next = vertices[i]!;
    out.push({ x: next.x, y: previous.y }, next);
  }
  return out;
}

/**
 * Build a pixel-bounded plot for the visible window. One neighboring sample on
 * either side is retained so lines enter/leave the viewport at the right
 * value. Nulls split segments; a step segment carries its prior value only up
 * to the null timestamp.
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
    };
  }

  const from = Math.max(0, lowerBound(samples, viewStart) - 1);
  const to = Math.min(
    samples.length,
    upperBound(samples, viewEnd) + (kind === "gauge" ? 1 : 0),
  );
  let min: FieldChartNumeric | null = null;
  let max: FieldChartNumeric | null = null;
  for (let i = from; i < to; i++) {
    const sample = samples[i]!;
    if (sample.value === null) continue;
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
    };
  }

  if (kind === "updown-counter") {
    const zero = zeroFor(min);
    if (compare(zero, min) < 0) min = zero;
    if (compare(zero, max) > 0) max = zero;
  }
  const chartH = Math.max(1, chartBottom - chartTop);
  const yAt = (value: FieldChartNumeric): number =>
    chartBottom - ratio(value, min!, max!) * chartH;
  const baselineY =
    kind === "updown-counter" ? yAt(zeroFor(min)) : chartBottom;

  const segments: FieldChartVertex[][] = [];
  const resetXs = new Set<number>();
  let rawSegment: RawPoint[] = [];
  let bucket: Bucket | null = null;

  const flushBucket = (): void => {
    if (bucket === null) return;
    rawSegment.push(...bucketPoints(bucket));
    bucket = null;
  };

  const flushSegment = (carryTo: number | null): void => {
    flushBucket();
    if (rawSegment.length === 0) return;
    let vertices = rawSegment.map((point) => ({
      x: xAt(point.timestamp, viewStart, viewEnd, drawW),
      y: yAt(point.value),
    }));
    if (kind !== "gauge") {
      vertices = stepAfter(vertices);
      if (carryTo !== null) {
        const x = xAt(carryTo, viewStart, viewEnd, drawW);
        const last = vertices[vertices.length - 1]!;
        if (x > last.x) vertices.push({ x, y: last.y });
      }
    }
    segments.push(vertices);
    rawSegment = [];
  };

  for (let i = from; i < to; i++) {
    const sample = samples[i]!;
    if (sample.value === null) {
      flushSegment(sample.timestamp);
      if (
        sample.gap === "reset" &&
        sample.timestamp >= viewStart &&
        sample.timestamp <= viewEnd
      ) {
        resetXs.add(
          pixelColumn(
            xAt(sample.timestamp, viewStart, viewEnd, drawW),
            drawW,
          ),
        );
      }
      continue;
    }
    const point: RawPoint = {
      index: i,
      timestamp: sample.timestamp,
      value: sample.value,
    };
    const x = xAt(sample.timestamp, viewStart, viewEnd, drawW);
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
  }
  flushSegment(kind === "gauge" ? null : viewEnd);

  return {
    segments,
    resetXs: [...resetXs],
    min,
    max,
    baselineY,
  };
}

function path(
  ctx: CanvasRenderingContext2D,
  vertices: readonly FieldChartVertex[],
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
  segments: readonly (readonly FieldChartVertex[])[],
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
  segments: readonly (readonly FieldChartVertex[])[],
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

function compactValue(value: FieldChartNumeric, unit: string | null): string {
  const raw = typeof value === "bigint" ? compactBigInt(value) : String(value);
  if (unit !== null) {
    if (!["ns", "us", "ms", "s", "bytes"].includes(unit)) return raw;
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
  if (typeof value === "bigint") return raw;
  if (value === 0) return "0";
  const abs = Math.abs(value);
  if (abs >= 1e9 || abs < 0.001) return value.toExponential(3);
  return String(Number(value.toPrecision(5)));
}

function clippedLabel(value: FieldChartNumeric, unit: string | null): string {
  const text = compactValue(value, unit);
  return text.length <= 22 ? text : `${text.slice(0, 21)}\u2026`;
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
  const kind =
    spec.kind === "updown-counter"
      ? "Up/down"
      : spec.kind[0]!.toUpperCase() + spec.kind.slice(1);
  ctx.fillText(
    `${kind} \u00b7 ${series.numericSampleCount} samples`,
    Math.max(0, drawW - 5),
    11,
  );
  if (series.numericSampleCount === 0) {
    ctx.textAlign = "center";
    ctx.fillText("No numeric samples", drawW / 2, height / 2);
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

/** Create the one controller/cache shared by all dynamic tracks in this view. */
export function createFieldChartTrack(
  store: ViewerStore,
): FieldChartTrackController {
  const cache = createFieldChartSeriesCache();
  const sizers = new WeakMap<
    HTMLCanvasElement,
    CanvasSizer<CanvasRenderingContext2D>
  >();

  return {
    reconcile(specs) {
      cache.reconcile(specs);
    },
    paint(canvas, geometry, spec, viewStart, viewEnd) {
      const trace = store.getState().trace.trace;
      if (trace === null) return;
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
      renderFieldChart(
        ctx,
        geometry,
        cache.get(trace, spec),
        spec,
        viewStart,
        viewEnd,
      );
    },
    dispose() {
      cache.clear();
    },
  };
}
