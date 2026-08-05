// The zero-baseline value scale, shared by every count-series chart.
//
// Issue #282: a 0-valued series (a global queue that never backs up, an idle
// runtime's task count) rendered as a 1px stroke fused to the axis —
// indistinguishable from "no data". The fix is a scale that reserves
// ZERO_BASELINE_PX between the chart's true bottom and the y that 0 maps to, so
// a flat zero reads as a flat LINE. The underlying numbers are unchanged; only
// the y-mapping is.
//
// This lives in lib/canvas rather than beside one chart because the queue track
// and the lanes' per-runtime summary lane plot the same kind of series and must
// not disagree about where zero is: a reader comparing the two would otherwise
// see one flat line and one blank band for the same data.

/**
 * Pixels reserved between the chart's true bottom (the axis) and the y a value
 * of 0 maps to, so a 0-valued series renders as a VISIBLE flat line this far
 * above the axis instead of a stroke fused to it. Small enough to cost almost
 * no vertical range, large enough to read as a distinct line.
 */
export const ZERO_BASELINE_PX = 3;

/**
 * Map a count-series value in [0, max] to a y coordinate inside the chart band
 * [chartTop, chartTop + chartH]. The bottom ZERO_BASELINE_PX is reserved so
 * value 0 lands a visible distance ABOVE the axis (the explicit zero baseline):
 * `queueScaleY(0, ...)` is strictly less than `chartTop + chartH`. `max` <= 0
 * pins everything to that baseline. The result is clamped into the band.
 *
 * This is the single scale every such series runs through — the queue track's
 * global / max-local / active-task lines and the lanes' per-runtime summary —
 * so a 0-valued series always renders a visible flat line.
 */
export function queueScaleY(
  value: number,
  max: number,
  chartTop: number,
  chartH: number,
  baselinePx: number = ZERO_BASELINE_PX,
): number {
  const reserve = Math.min(Math.max(0, baselinePx), chartH);
  const usableH = chartH - reserve;
  const baselineY = chartTop + chartH - reserve;
  if (!(max > 0) || usableH <= 0) return baselineY;
  const norm = value <= 0 ? 0 : value >= max ? 1 : value / max;
  return baselineY - norm * usableH;
}

/** The y a value of 0 maps to (the visible zero baseline). */
export function queueBaselineY(
  chartTop: number,
  chartH: number,
  baselinePx: number = ZERO_BASELINE_PX,
): number {
  return chartTop + chartH - Math.min(Math.max(0, baselinePx), chartH);
}
