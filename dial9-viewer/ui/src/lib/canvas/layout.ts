// Typed wrapper around the frozen core's panel_layout.js.
//
// Time-panel layout invariant: every time-based panel splits its width as
// label gutter (LABEL_W) + draw area (drawW) + scrollbar (scrollbarW),
// with drawW = pw - LABEL_W - scrollbarW, so all panels map the same
// timestamp to the same x and their time axes line up vertically. A panel
// that redefines the gutter silently shifts its axis relative to every
// other panel.
//
// This module is the single producer of layout geometry in src/: every
// canvas component receives a PanelGeometry built here, and nothing else
// imports panel_layout.js directly. The wrapper is pure - callers pass
// measured widths in - so DOM reads batch once per frame instead of
// interleaving with draws.

import { makeTimePanelLayout } from "../../../panel_layout.js";
import type { TimePanelLayout } from "../../../panel_layout.js";
import type {
  LaneGeometry,
  PanelGeometry,
  PanelKind,
} from "../../types/state.js";

export type { TimePanelLayout };

/**
 * The canonical left-gutter width (CSS px) reserved for labels in every
 * time-based panel. The invariant is that ALL panels use this one
 * constant, never a private gutter width.
 */
export const LABEL_W = 100;

export interface TimePanelLayoutOpts {
  /** Full panel/canvas width in CSS px (the panel's clientWidth). */
  pw: number;
  /**
   * Right gutter matching the lanes-area vertical scrollbar, so the draw
   * area's right edge lines up with the worker lanes. Omit (or 0) for
   * panels that don't need to match the lane right edge.
   */
  scrollbarW?: number;
  /** Visible-range start timestamp (ns). */
  viewStart: number;
  /** Visible-range end timestamp (ns). */
  viewEnd: number;
}

/**
 * Build the shared ns<->x mapping for one time panel, with the LABEL_W
 * gutter applied. Thin typed wrapper over the frozen core's
 * makeTimePanelLayout - the math (including the zero-span guard) lives
 * there.
 *
 * `drawW` can come out <= 0 on very narrow panels; callers are expected
 * to early-return in that case.
 */
export function timePanelLayout(opts: TimePanelLayoutOpts): TimePanelLayout {
  return makeTimePanelLayout(
    opts.pw,
    LABEL_W,
    opts.scrollbarW,
    opts.viewStart,
    opts.viewEnd,
  );
}

export interface PanelGeometryOpts extends TimePanelLayoutOpts {
  kind: PanelKind;
  /** Panel canvas height in CSS px. */
  height: number;
  /** Device pixel ratio the backing store is scaled by. */
  dpr: number;
}

/**
 * Build the full geometry handed to a canvas component's
 * `render(ctx, state, layout)`: the shared time mapping plus this panel's
 * box.
 */
export function panelGeometry(opts: PanelGeometryOpts): PanelGeometry {
  return {
    kind: opts.kind,
    time: timePanelLayout(opts),
    height: opts.height,
    dpr: opts.dpr,
  };
}

/**
 * Geometry of the worker-lane rows as a vertical stack: row `i` sits at
 * y = i * laneHeight, in the order `workerIds` is given. `y` is
 * lanes-local (before scroll offset).
 */
export function laneStackGeometry(
  workerIds: readonly number[],
  laneHeight: number,
): LaneGeometry[] {
  return workerIds.map((workerId, index) => ({
    workerId,
    index,
    y: index * laneHeight,
    height: laneHeight,
  }));
}
