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
import type { PanelGeometry, PanelKind } from "../../types/state.js";
import type { RuntimeGroup } from "../../types/trace.js";

export type { TimePanelLayout };

/**
 * The canonical left-gutter width (CSS px) reserved for labels in every
 * time-based panel. The invariant is that ALL panels use this one
 * constant, never a private gutter width.
 */
export const LABEL_W = 100;

/**
 * Timestamp (ns) -> draw-area-relative x (px). THE alignment invariant: every
 * time-based track maps ns to x with this one expression, so ticks, poll bars,
 * span bars and CPU bars line up pixel-exact. No LABEL_W is added - a track
 * canvas already sits after the DOM label gutter.
 *
 * Deliberately UNCLAMPED, so callers that need to know a mark fell outside the
 * viewport still can. Compose with clampX where the x feeds a fillRect.
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

/** Clamp a draw-area x into [0, drawW]. */
export function clampX(x: number, drawW: number): number {
  return x < 0 ? 0 : x > drawW ? drawW : x;
}

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

/** A row in the vertical lanes stack: a fixed-height worker row, or a runtime
 *  group header band. `y`/`height` are lanes-local CSS px (before scroll). */
export type LaneRow =
  | {
      kind: "worker";
      workerId: number;
      /** Position in the flat worker order (top = 0), for callers that key by it. */
      index: number;
      y: number;
      height: number;
    }
  | {
      kind: "header";
      name: string;
      /** True for the inferred default ("main") runtime (label reads differently). */
      inferred: boolean;
      workerCount: number;
      /** True when this runtime is folded: the header is drawn but its worker
       *  rows are omitted from the stack. Drives the header caret direction. */
      collapsed: boolean;
      y: number;
      height: number;
    };

export interface LaneRowLayout {
  /** Interleaved header + worker rows, top to bottom. */
  rows: LaneRow[];
  /** Total stacked height (last row's `y + height`); the scroll content height. */
  contentHeight: number;
}

/**
 * The runtime-aware vertical layout of the lanes stack: each runtime group emits
 * a header row (only when there is MORE than one group - the single-runtime
 * common case stays header-free) followed by its workers at fixed `rowH`.
 * Cumulative `y` runs top to bottom. This is the ONE source of lane vertical
 * geometry - the renderer draws from it and both hit-tests resolve against it,
 * so a fixed row height + headers can never drift between draw and click.
 *
 * A group named in `collapsed` (only honoured when headers are shown) keeps its
 * header but omits its worker rows, so a folded runtime takes only header
 * height. The flat worker `index` counts emitted rows only, so it stays a dense,
 * unique per-frame batcher key regardless of which groups are folded.
 */
export function laneRowLayout(
  groups: readonly RuntimeGroup[],
  rowH: number,
  headerH: number,
  collapsed: Readonly<Record<string, boolean>> = {},
): LaneRowLayout {
  const rows: LaneRow[] = [];
  const showHeaders = groups.length > 1;
  let y = 0;
  let index = 0;
  for (const g of groups) {
    const isCollapsed = showHeaders && collapsed[g.name] === true;
    if (showHeaders) {
      rows.push({
        kind: "header",
        name: g.name,
        inferred: g.inferred,
        workerCount: g.workerIds.length,
        collapsed: isCollapsed,
        y,
        height: headerH,
      });
      y += headerH;
    }
    if (isCollapsed) continue;
    for (const workerId of g.workerIds) {
      rows.push({ kind: "worker", workerId, index, y, height: rowH });
      y += rowH;
      index++;
    }
  }
  return { rows, contentHeight: y };
}

/**
 * Resolve a lanes-local y (client y minus the viewport top PLUS the box
 * scrollTop) to the worker id whose row contains it, or null when the point is
 * over a header band or past the content. Linear over the rows (a lanes stack is
 * small); shared by the click + hover hit-tests so both honor the same
 * fixed-height + header geometry.
 */
export function workerAtLaneY(rowLayout: LaneRowLayout, localY: number): number | null {
  if (localY < 0) return null;
  for (const row of rowLayout.rows) {
    if (localY < row.y || localY >= row.y + row.height) continue;
    return row.kind === "worker" ? row.workerId : null;
  }
  return null;
}

/**
 * Resolve a lanes-local y to the runtime-group NAME whose header band contains
 * it, or null when the point is over a worker row / past the content. The
 * counterpart to workerAtLaneY over the same geometry, so a click resolves to
 * exactly one target (a header toggle or a worker select, never both).
 */
export function headerAtLaneY(rowLayout: LaneRowLayout, localY: number): string | null {
  if (localY < 0) return null;
  for (const row of rowLayout.rows) {
    if (localY < row.y || localY >= row.y + row.height) continue;
    return row.kind === "header" ? row.name : null;
  }
  return null;
}
