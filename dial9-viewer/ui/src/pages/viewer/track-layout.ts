// src/pages/viewer/track-layout.ts - the viewer shell's track catalogue and
// per-track geometry (T21; layout decision docs/tickets/chunk-2-viewer.md
// header, mocks docs/ui-inventory/mocks/concept-{1,2}.html).
//
// The DECIDED layout is a single time-aligned scrolling column: the worker
// lanes on top, then every analysis surface as its own labeled track sharing
// ONE time axis (concept-1). The old foldable panels die (amendment S1 -
// analysis surfaces are visible by default); each track keeps a left label
// area of LABEL_W and an empty draw canvas whose width comes from
// lib/canvas/layout (the frozen panel_layout.js invariant), so every track's
// time axis lines up vertically with the lanes.
//
// T21 owns the SLOTS only: each track renders its label + a correctly-sized
// empty canvas (the "placeholder" contract in the DoD). The CONTENT is filled
// by later tickets - the seams are recorded per track below so nothing here
// precludes them:
//   timeline axis  -> T25   worker lanes -> T22 (+ interaction T23)
//   spans          -> T26   custom events -> T27
//   cpu            -> T28    queue        -> T29
//   task-detail    -> T30 (shown only while a task is selected)
//
// Heights are seeded from the concept mocks / legacy expanded panel heights
// (features/02 J/K/L/M/N). They are the shell's starting geometry; a track
// component may refine its own height when it lands (T36 also adds
// collapse/reorder), but the axis math (x-mapping) is fixed here for all.

import { panelGeometry, LABEL_W } from "../../lib/canvas/layout.js";
import type { PanelGeometry, PanelKind } from "../../types/state.js";

export { LABEL_W };

/**
 * Every track slot in the unified column, in top-to-bottom render order.
 * "timeline" and "lanes" are the two structural tracks above the analysis
 * surfaces; "timeline" is not a PanelKind (it is the shared axis header,
 * T25) and "lanes" hosts the worker rows (T22), so they carry their own
 * ids while the analysis tracks reuse the PanelKind vocabulary.
 */
export type TrackId = "timeline" | "lanes" | PanelKind;

export interface TrackSpec {
  id: TrackId;
  /** Human label shown in the track's left gutter / header. */
  label: string;
  /** Starting track height in CSS px (draw-area height for the canvas). */
  height: number;
  /**
   * The ticket that fills this slot's content, recorded so the seam is
   * explicit (T21 renders the empty placeholder only).
   */
  ownedBy: string;
  /**
   * Shown only while a task is selected (task-detail, features/02 N1); the
   * shell keeps its slot in the DOM but hidden until selection drives it.
   */
  selectionOnly?: boolean;
}

/**
 * The canonical track catalogue. Order is the concept-1 column order:
 * axis, lanes, then the analysis surfaces top-down. Task detail is last
 * (it appears below the surfaces when a task is selected, features/02 N1).
 */
export const TRACKS: readonly TrackSpec[] = [
  { id: "timeline", label: "Time", height: 30, ownedBy: "T25" },
  { id: "lanes", label: "Workers", height: 130, ownedBy: "T22" },
  { id: "cpu", label: "CPU", height: 74, ownedBy: "T28" },
  { id: "queue", label: "Queue G+L", height: 74, ownedBy: "T29" },
  { id: "spans", label: "Spans", height: 150, ownedBy: "T26" },
  // Events track (T27): a legend strip (LEGEND_H) above a marker-tick canvas.
  // Height seats the legacy 40px tick canvas plus the chip legend.
  { id: "events", label: "Events", height: 70, ownedBy: "T27" },
  {
    id: "task-detail",
    label: "Task detail",
    height: 160,
    ownedBy: "T30",
    selectionOnly: true,
  },
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
 * Map a TrackId to the PanelKind whose geometry it borrows. The two
 * structural tracks have no PanelKind of their own, but they share the
 * exact LABEL_W + drawW split, so they map onto an analysis kind purely to
 * reuse panelGeometry (the `kind` field is advisory - the x-mapping is
 * identical across all kinds by the A13 invariant).
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
