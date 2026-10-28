import type { CategoricalHistogram, CategoryLatency } from "../../lib/trace/span-tail-analysis.js";

export interface HistogramRange {
  minMs: number | null;
  maxMs: number | null;
}

const clamp = (value: number, low: number, high: number) =>
  Math.max(low, Math.min(high, value));

/**
 * Histogram columns have equal screen widths. The zero-inclusive first
 * column is linear; positive columns interpolate logarithmically.
 */
export function latencyToHistogramPosition(
  edges: readonly number[],
  latencyMs: number,
): number {
  const bins = edges.length - 1;
  if (bins < 1) return 0;
  if (latencyMs <= edges[0]!) return 0;
  if (latencyMs >= edges[bins]!) return bins;
  const bin = edges.findIndex((upper, i) => i > 0 && upper >= latencyMs) - 1;
  const lower = edges[bin]!;
  const upper = edges[bin + 1]!;
  const fraction = lower === 0
    ? latencyMs / upper
    : Math.log(latencyMs / lower) / Math.log(upper / lower);
  return bin + clamp(fraction, 0, 1);
}

export function histogramPositionToLatency(
  edges: readonly number[],
  position: number,
): number {
  const bins = edges.length - 1;
  const bounded = clamp(position, 0, bins);
  const bin = Math.min(bins - 1, Math.floor(bounded));
  const fraction = bounded - bin;
  const lower = edges[bin]!;
  const upper = edges[bin + 1]!;
  return lower === 0
    ? upper * fraction
    : lower * (upper / lower) ** fraction;
}

/** An edge-to-edge brush represents the complete duration range. */
export function histogramBrushRange(
  edges: readonly number[],
  from: number,
  to: number,
): HistogramRange {
  const bins = edges.length - 1;
  const lower = clamp(Math.min(from, to), 0, bins);
  const upper = clamp(Math.max(from, to), 0, bins);
  return {
    minMs: lower === 0 ? null : histogramPositionToLatency(edges, lower),
    maxMs: upper === bins ? null : histogramPositionToLatency(edges, upper),
  };
}

/** Missing cached bin counts remain unavailable rather than reconstructed. */
export function categoryBinCount(
  value: CategoryLatency & { binCounts?: readonly number[] },
  bin: number,
): number | null {
  const exact = value.binCounts?.[bin];
  return exact != null && Number.isSafeInteger(exact) && exact >= 0 ? exact : null;
}

export type CategoricalStack =
  | {
    available: true;
    population: number;
    totalCounts: number[];
    totalShares: number[];
    layers: { lower: number[]; upper: number[] }[];
    maximum: number;
  }
  | {
    available: false;
    reason: "missing-counts" | "incomplete-values" | "invalid-counts";
  };

/**
 * Stack exact backend bin counts as shares of the complete recorded-value
 * population. Missing property values are outside that population.
 */
export function categoricalHistogramStack(histogram: CategoricalHistogram): CategoricalStack {
  const bins = histogram.binEdgesMs.length - 1;
  if (!Number.isSafeInteger(histogram.count) || histogram.count <= 0 || bins < 1) {
    return { available: false, reason: "invalid-counts" };
  }
  if (histogram.values.some(value => value.binCounts == null)) {
    return { available: false, reason: "missing-counts" };
  }
  if (histogram.values.some(value =>
    !Number.isSafeInteger(value.count) || value.count < 0
    || value.binCounts!.length !== bins
    || value.binCounts!.some(count => !Number.isSafeInteger(count) || count < 0)
    || value.binCounts!.reduce((total, count) => total + count, 0) !== value.count
  )) {
    return { available: false, reason: "invalid-counts" };
  }
  const representedCount = histogram.values.reduce((total, value) => total + value.count, 0);
  if (representedCount < histogram.count) {
    // There are no bin counts for an omitted "Other" population.
    return { available: false, reason: "incomplete-values" };
  }
  if (representedCount > histogram.count) {
    return { available: false, reason: "invalid-counts" };
  }
  let cumulative = Array<number>(bins).fill(0);
  const layers = histogram.values.map(value => {
    const lower = cumulative.map(count => count / histogram.count);
    cumulative = cumulative.map((count, bin) => count + value.binCounts![bin]!);
    return { lower, upper: cumulative.map(count => count / histogram.count) };
  });
  const totalShares = cumulative.map(count => count / histogram.count);
  return {
    available: true,
    population: histogram.count,
    totalCounts: cumulative,
    totalShares,
    layers,
    maximum: Math.max(...totalShares),
  };
}

/** Close each stepped area along its own cumulative lower boundary. */
export function histogramStackArea(
  lower: readonly number[],
  upper: readonly number[],
  maximum: number,
): string {
  let path = `M0 ${1 - upper[0]! / maximum}`;
  upper.forEach((share, bin) => {
    path += `V${1 - share / maximum}H${bin + 1}`;
  });
  path += `V${1 - lower.at(-1)! / maximum}`;
  for (let bin = lower.length - 1; bin >= 0; bin--) {
    path += `V${1 - lower[bin]! / maximum}H${bin}`;
  }
  return `${path}Z`;
}

/** Keep same-position percentile labels distinguishable in narrow charts. */
export function histogramMarkerRows(positions: readonly number[]): number[] {
  const rows: number[] = [];
  const rowEnds: number[] = [];
  const order = positions.map((position, index) => ({ position, index }))
    .sort((a, b) => a.position - b.position);
  for (const { position, index } of order) {
    // A fixed quarter of the chart keeps short labels apart on mobile too.
    let row = rowEnds.findIndex(end => position - end >= .25);
    if (row < 0) row = rowEnds.length;
    rowEnds[row] = position;
    rows[index] = row;
  }
  return rows;
}
