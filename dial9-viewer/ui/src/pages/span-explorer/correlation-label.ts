/** Descriptive Spearman labels; these do not establish a cause. */
export function correlationLabel(coefficient: number | null): string {
  if (coefficient == null || !Number.isFinite(coefficient)) return "Correlation unavailable";
  if (coefficient === 1) return "Perfectly correlated";
  if (coefficient === -1) return "Perfectly inversely correlated";
  const strength = Math.abs(coefficient);
  if (strength < .1) return "Not correlated";
  const prefix = strength >= .7 ? "Highly" : strength >= .3 ? "Moderately" : "Weakly";
  return `${prefix} ${coefficient < 0 ? "inversely " : ""}correlated`;
}
