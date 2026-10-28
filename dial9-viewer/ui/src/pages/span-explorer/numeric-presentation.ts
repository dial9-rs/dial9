import { formatHumanDuration } from "../../lib/trace/api_format.js";
import { latencyToHistogramPosition } from "./categorical-histogram-scale.js";

export const number = (n: number) => n !== 0 && Math.abs(n) < .001
  ? n.toPrecision(3) : n.toLocaleString(undefined, { maximumFractionDigits: 3 });
export const duration = (ms: number) => formatHumanDuration(ms * 1e6);
export const axisNumber = (n: number) => n !== 0 && (Math.abs(n) < .001 || Math.abs(n) >= 1e5)
  ? n.toExponential(1).replace("e+", "e") : number(n);
export function numericAxisTick(n: number, unit: string | null): string {
  if (unit !== "bytes" || Math.abs(n) < 1e3 || Math.abs(n) >= 1e21) return axisNumber(n);
  const prefixes = ["kB", "MB", "GB", "TB", "PB", "EB"];
  const power = Math.min(prefixes.length, Math.floor(Math.log10(Math.abs(n)) / 3));
  const amount = Number((n / 1000 ** power).toPrecision(3));
  return `${amount} ${prefixes[power - 1]}`;
}
export const fieldValue = (n: number, unit: string | null) => unit === "bytes" && Math.abs(n) >= 1e3
  ? numericAxisTick(n, unit) : `${number(n)}${unit ? ` ${unit}` : " (unit not recorded)"}`;

/** Split at exact display positions without inventing counts within a cell. */
export function densityCellSegments(
  edges: readonly number[], bin: number, lowerMs: number, upperMs: number | null,
): { start: number; end: number; selected: boolean }[] {
  const lower = latencyToHistogramPosition(edges, lowerMs);
  const upper = upperMs == null ? edges.length - 1 : latencyToHistogramPosition(edges, upperMs);
  const cuts = [bin, ...[lower, upper].filter(n => n > bin && n < bin + 1), bin + 1];
  const sorted = [...new Set(cuts)].sort((a, b) => a - b);
  return sorted.slice(0, -1).map((start, index) => {
    const end = sorted[index + 1]!;
    return { start, end, selected: start >= lower && end <= upper };
  });
}
