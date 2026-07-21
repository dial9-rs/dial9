// The viewer shell's track catalogue and per-track geometry.
//
// The layout is a single time-aligned scrolling column: the worker lanes on
// top, then every analysis surface as its own labeled track sharing ONE time
// axis. Each track keeps a left label area of LABEL_W and a draw canvas whose
// width comes from lib/canvas/layout, so every track's time axis lines up
// vertically with the lanes. Heights are the shell's starting geometry; a track
// component may refine its own height, but the axis math (x-mapping) is fixed
// here for all.

import { panelGeometry, LABEL_W } from "../../lib/canvas/layout.js";
import type { PanelGeometry, PanelKind } from "../../types/state.js";

export { LABEL_W };

/** CSS class of the resizeable worker-lanes scroll box. */
export const LANES_VIEWPORT_CLASS = "d9-lanes-viewport";

/**
 * Width (CSS px) of the worker-lanes box vertical scrollbar, measured live, or 0
 * when the box is absent / uses overlay scrollbars. The lanes box (not the track
 * column) owns the scrollbar that every track's draw area must clear, so ALL
 * tracks reserve this as their right gutter and their time axes line up with the
 * lanes. `scrollbar-gutter: stable` on the box keeps this constant regardless of
 * whether the box is currently scrolling, so alignment never flickers.
 */
export function lanesScrollbarWidth(columnEl: HTMLElement): number {
  const box = columnEl.querySelector<HTMLElement>(`.${LANES_VIEWPORT_CLASS}`);
  if (!box) return 0;
  return Math.max(0, box.offsetWidth - box.clientWidth);
}

/**
 * Every track slot in the unified column, in top-to-bottom render order.
 * "timeline" and "lanes" are the two structural tracks above the analysis
 * surfaces; "timeline" is not a PanelKind (it is the shared axis header) and
 * "lanes" hosts the worker rows, so they carry their own ids while the analysis
 * tracks reuse the PanelKind vocabulary.
 */
export type TrackId = "timeline" | "lanes" | PanelKind;

export interface TrackSpec {
  id: TrackId;
  /** Human label shown in the track's left gutter / header. */
  label: string;
  /** Starting track height in CSS px (draw-area height for the canvas). */
  height: number;
  /**
   * Shown only while a task is selected (task-detail); the shell keeps its slot
   * in the DOM but hidden until selection drives it.
   */
  selectionOnly?: boolean;
}

/**
 * The canonical track catalogue, in column order: axis, lanes, the selected
 * task's detail, then the analysis surfaces top-down.
 *
 * Task detail sits directly under the worker lanes rather than at the bottom:
 * it is the surface a user reads immediately after clicking a task, and below
 * four expanded analysis tracks it fell past the scroll fold, so selecting a
 * task appeared to do nothing. It stays selectionOnly, so it still occupies no
 * space until a task is actually selected.
 */
export const TRACKS: readonly TrackSpec[] = [
  { id: "timeline", label: "Time", height: 30 },
  { id: "lanes", label: "Workers", height: 130 },
  {
    id: "task-detail",
    label: "Task detail",
    height: 160,
    selectionOnly: true,
  },
  { id: "cpu", label: "CPU", height: 74 },
  { id: "queue", label: "Queue G+L", height: 74 },
  { id: "spans", label: "Spans", height: 150 },
  // Events track: a legend strip (LEGEND_H) above a marker-tick canvas. Height
  // seats a 40px tick canvas plus the chip legend.
  { id: "events", label: "Events", height: 70 },
];

export interface TrackGeometryOpts {
  /** Full track/canvas width in CSS px (the column's clientWidth). */
  pw: number;
  /** Right gutter matching the lanes vertical scrollbar (0 if none). */
  scrollbarW?: number;
  /** Visible-range start timestamp (ns). */
  viewStart: number;
  /** Visible-range end timestamp (ns). */
  viewEnd: number;
  /** Device pixel ratio the backing store is scaled by. */
  dpr: number;
}

/**
 * Build the PanelGeometry for one track from the shared layout math. The
 * "timeline" and "lanes" structural tracks are mapped onto the nearest
 * PanelKind for geometry purposes (they use the same LABEL_W gutter + drawW
 * split), so a single code path produces every track's aligned x-mapping.
 * Callers early-return when `geometry.time.drawW <= 0` (narrow-panel
 * contract, inherited from lib/canvas/layout).
 */
export function trackGeometry(
  track: TrackSpec,
  opts: TrackGeometryOpts,
): PanelGeometry {
  const kind: PanelKind = geometryKindFor(track.id);
  return panelGeometry({
    kind,
    pw: opts.pw,
    ...(opts.scrollbarW !== undefined ? { scrollbarW: opts.scrollbarW } : {}),
    viewStart: opts.viewStart,
    viewEnd: opts.viewEnd,
    height: track.height,
    dpr: opts.dpr,
  });
}

/**
 * Map a TrackId to the PanelKind whose geometry it borrows. The two structural
 * tracks have no PanelKind of their own, but they share the exact LABEL_W +
 * drawW split, so they map onto an analysis kind purely to reuse panelGeometry
 * (the `kind` field is advisory - the x-mapping is identical across all kinds).
 */
function geometryKindFor(id: TrackId): PanelKind {
  switch (id) {
    case "timeline":
    case "lanes":
    case "spans":
      return "spans";
    case "cpu":
      return "cpu";
    case "queue":
      return "queue";
    case "events":
      return "events";
    case "task-detail":
      return "task-detail";
  }
}
