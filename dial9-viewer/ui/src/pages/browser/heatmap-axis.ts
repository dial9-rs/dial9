// Pure layout model for the browse timeline's time axis and hover readout.
//
// One tick set drives two renderers - the DOM tick labels under the plot and
// the full-height gridlines drawn on the canvas - so a label always sits on
// the line it belongs to. Tick TIMES come from the frozen core's
// niceTimeTicks, which snaps them to round wall-clock instants; this module
// only adds pixel placement and labelling, and stays DOM-free so both are
// unit-testable.

import { niceTimeTicks } from "../../lib/canvas/heatmap.js";
import {
  clamp,
  crossesDayBoundary,
  fmtTick,
  formatEpochStr,
  timeToX,
  xToTime,
} from "./format.js";
import type { TimeDomain } from "./state.js";

/** Roughly one tick per 130px, never fewer than 2 nor more than 8. */
export function tickTarget(W: number): number {
  return clamp(Math.floor(W / 130), 2, 8);
}

export interface AxisTick {
  /** Tick time, epoch seconds. */
  t: number;
  /** Pixel offset from the canvas' left edge. */
  x: number;
  /** Rendered tick text. */
  label: string;
}

/**
 * Ticks for `domain` across a canvas `W` pixels wide, in the active TZ mode.
 *
 * The leftmost tick always carries its calendar date, and every tick carries
 * one when the visible span crosses a day boundary - a bare HH:MM:SS on a
 * multi-day window cannot say which day it means. Ticks outside [0, W] are
 * dropped so a caller never has to re-check bounds.
 */
export function axisTicks(domain: TimeDomain, W: number, localTz: boolean): AxisTick[] {
  const { tMin, tMax } = domain;
  const everyTickDated = crossesDayBoundary(tMin, tMax, localTz);
  // A zero-width (or inverted) domain makes timeToX a 0/0: niceTimeTicks
  // returns the single tick tMin, which belongs at the left edge.
  const degenerate = !(tMax > tMin);
  const out: AxisTick[] = [];
  const times = niceTimeTicks(tMin, tMax, tickTarget(W));
  times.forEach((t, i) => {
    const x = degenerate ? 0 : timeToX(t, tMin, tMax, W);
    if (x < 0 || x > W) return;
    const withDate = everyTickDated || i === 0;
    out.push({ t, x, label: fmtTick(t, localTz, withDate) });
  });
  return out;
}

export interface CursorPlacement {
  /** Left offset of the 1px cursor line. */
  lineLeft: number;
  /** Left offset of the (center-anchored) time label. */
  labelLeft: number;
}

/** The full timestamp under a pointer at `x` over a canvas `W` px wide. */
export function timestampAt(
  x: number,
  domain: TimeDomain,
  W: number,
  localTz: boolean,
): string {
  return formatEpochStr(xToTime(x, domain.tMin, domain.tMax, W), localTz);
}

/**
 * Where to put the hover cursor line and its time label for a pointer at `x`.
 *
 * The label is center-anchored (translateX(-50%)), so its left offset is
 * pulled in by half its width at each edge to keep it inside the plot -
 * measure the label AFTER writing its text, or the clamp uses a stale width.
 * A label wider than the plot itself is simply centered: no offset fits, and
 * centering clips both ends equally rather than hiding one outright.
 */
export function cursorPlacement(
  x: number,
  W: number,
  labelWidth: number,
): CursorPlacement {
  const half = labelWidth / 2;
  return {
    lineLeft: Math.round(x),
    labelLeft: labelWidth >= W ? W / 2 : clamp(x, half, W - half),
  };
}
