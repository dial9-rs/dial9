import type { TailSignal } from "../../lib/trace/span-tail-analysis.js";

/** Qualitative display of the backend's conservative cohort-distribution gap. */
export function tailDifferenceLabel(signal: TailSignal, selected = false): string {
  if (Math.min(signal.typicalCount, signal.slowCount) < 20) return "Too few values to compare";
  const difference = signal.tailDifference;
  if (difference == null || !Number.isFinite(difference.lowerBound)) return "Tail comparison unavailable";
  const strength = difference.lowerBound;
  const adjective = strength >= .3 ? "Strong" : strength >= .1 ? "Moderate" : strength > 0 ? "Small" : "No clear";
  return `${adjective} difference in ${selected ? "selected" : "slow"} spans`;
}
