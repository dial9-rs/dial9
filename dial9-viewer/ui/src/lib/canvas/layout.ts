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
    }
  | {
      /** A per-runtime summary lane (global queue + alive tasks for the
       *  runtime), rendered as the group's FOOTER - directly under its worker
       *  rows - and folded away with the runtime. Emitted only for groups named
       *  in `metrics.runtimes`. */
      kind: "runtime-metrics";
      /** Group name, used to look up the runtime's metric series. */
      name: string;
      /** True for the inferred default ("main") runtime (its metrics are keyed
       *  under the empty wire name). */
      inferred: boolean;
      /** True when the user folded this summary lane down to its one-line
       *  strip (independently of the runtime's own fold). */
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

/** Which runtimes get a summary lane, and which of those the user has folded to
 *  the one-line strip. Passed as one object so the two always travel together
 *  (a name in `collapsed` but not `runtimes` has no lane to fold). */
export interface MetricsLaneOpts {
  /** Group names that have a metric series, so a summary lane is emitted. */
  runtimes: ReadonlySet<string>;
  /** Group name -> true when the user folded that runtime's summary lane to its
   *  one-line strip. Absent/false = the full chart. */
  collapsed?: Readonly<Record<string, boolean>>;
}

/** Height (CSS px) of a summary lane folded to its one-line strip: the title +
 *  the headline numbers, no chart. */
export const METRICS_LANE_COLLAPSED_H = 18;

/**
 * The runtime-aware vertical layout of the lanes stack: each runtime group emits
 * a header row (only when there is MORE than one group - the single-runtime
 * common case stays header-free), then its workers at fixed `rowH`, then - for a
 * group with a metric series - its summary lane as the group's FOOTER.
 * Cumulative `y` runs top to bottom. This is the ONE source of lane vertical
 * geometry - the renderer draws from it and both hit-tests resolve against it,
 * so a fixed row height + headers can never drift between draw and click.
 *
 * The summary lane sits UNDER its workers (not above): it is a per-runtime
 * total, so it reads as the group's bottom line, and a reader scanning the
 * worker rows meets the runtime's aggregate right where the group ends rather
 * than before its detail.
 *
 * A group named in `collapsed` (only honoured when headers are shown) keeps its
 * header but omits both its worker rows and its summary lane, so a folded
 * runtime takes only header height. A summary lane named in
 * `metrics.collapsed` shrinks to {@link METRICS_LANE_COLLAPSED_H} instead of
 * disappearing, so its headline numbers stay readable. The flat worker `index`
 * counts emitted rows only, so it stays a dense, unique per-frame batcher key
 * regardless of which groups are folded.
 */
export function laneRowLayout(
  groups: readonly RuntimeGroup[],
  rowH: number,
  headerH: number,
  collapsed: Readonly<Record<string, boolean>> = {},
  metrics: MetricsLaneOpts = EMPTY_METRICS_LANES,
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
    // The group's footer: its runtime summary lane, folded away with the runtime
    // and independently foldable to a one-line strip. Only for groups that
    // actually have a metric series.
    if (metrics.runtimes.has(g.name)) {
      const laneCollapsed = metrics.collapsed?.[g.name] === true;
      rows.push({
        kind: "runtime-metrics",
        name: g.name,
        inferred: g.inferred,
        collapsed: laneCollapsed,
        y,
        height: laneCollapsed ? METRICS_LANE_COLLAPSED_H : rowH,
      });
      y += laneCollapsed ? METRICS_LANE_COLLAPSED_H : rowH;
    }
  }
  return { rows, contentHeight: y };
}

const EMPTY_METRICS_LANES: MetricsLaneOpts = { runtimes: new Set() };

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

/**
 * Resolve a lanes-local y to the runtime NAME whose summary lane contains it, or
 * null when the point is over a worker row / header / past the content. The
 * third member of the same family as workerAtLaneY + headerAtLaneY over the same
 * geometry, so one point resolves to exactly one target: a worker select, a
 * runtime fold, or a summary-lane fold.
 */
export function metricsLaneAtLaneY(
  rowLayout: LaneRowLayout,
  localY: number,
): string | null {
  if (localY < 0) return null;
  for (const row of rowLayout.rows) {
    if (localY < row.y || localY >= row.y + row.height) continue;
    return row.kind === "runtime-metrics" ? row.name : null;
  }
  return null;
}
