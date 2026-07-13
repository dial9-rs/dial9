// components/canvas/lanes/index.ts - mount the worker-lanes track (T22).
//
// Wires render.ts to the viewer store: subscribes to the slices the lanes
// depend on (trace / viewport / selection) and redraws ONLY the lanes canvas
// when one of them changes (03 F2 - a selection change redraws lanes, not the
// span/cpu/queue panels; those are their own tickets with their own
// subscriptions). Frame-invariant lane data (buildWorkerSpans etc.) is
// computed through the store's derived() cache keyed by the `trace` slice, so
// pan and selection never re-derive it (03 F5).
//
// The lanes canvas lives in the shell's track column (T21). This mount claims
// the "lanes" track (track-renderers.ts) so the shell stops painting the
// placeholder over it, then owns the canvas's DPR sizing + draw.

import type { ViewerStore } from "../../../store/store.js";
import type { StoreState } from "../../../types/state.js";
import { createCanvasSizer } from "../../../lib/canvas/dpr.js";
import type { CanvasSizer } from "../../../lib/canvas/dpr.js";
import { makeColorDimmer } from "../../../lib/canvas/palette.js";
import { TRACKS, trackGeometry } from "../../../pages/viewer/track-layout.js";
import type { TrackId } from "../../../pages/viewer/track-layout.js";
import { claimTrack } from "../../../pages/viewer/track-renderers.js";
import { deriveLaneData } from "./data.js";
import type { LaneData } from "./data.js";
import {
  type LaneDrawContext,
  type LanesRenderInput,
  renderLanes,
  sharedVisibleMaxQueue,
} from "./render.js";
import { ensureLanesLegend, mountLanesLegend } from "./legend.js";

const LANES_TRACK_ID: TrackId = "lanes";

export interface MountedLanes {
  /** Force one redraw (used after mount / on resize). */
  refresh(): void;
  /** Tear down the subscription and release the track claim. */
  dispose(): void;
}

/**
 * Mount the worker-lanes renderer against `store`, drawing into the lanes
 * track canvas inside `trackColumn` (the shell's `.d9-track-column`).
 */
export function mountLanes(trackColumn: HTMLElement, store: ViewerStore): MountedLanes {
  const releaseClaim = claimTrack(LANES_TRACK_ID);
  const disposeLegend = mountLanesLegend(trackColumn);

  // Frame-invariant lane data, recomputed only when the trace slice is
  // replaced (F5). Non-null only once a trace has loaded.
  const laneData = store.derived(["trace"], (s): LaneData | null =>
    s.trace.trace ? deriveLaneData(s.trace.trace) : null,
  );

  // Non-selected poll dimmer, cached for the mount's lifetime (the transform
  // depends only on the <=24 quantized poll colors - millions of calls per
  // large-trace render collapse to a Map lookup, palette.ts contract).
  const dimmer = makeColorDimmer(0.4);

  let sizer: CanvasSizer<CanvasRenderingContext2D> | null = null;
  let sizerCanvas: HTMLCanvasElement | null = null;

  function laneCanvas(): HTMLCanvasElement | null {
    return trackColumn.querySelector<HTMLCanvasElement>(
      `canvas[data-track-canvas="${LANES_TRACK_ID}"]`,
    );
  }

  function draw(): void {
    const state = store.getState() as StoreState;
    const data = laneData();
    if (!data || data.workerIds.length === 0) return;
    const canvas = laneCanvas();
    if (!canvas) return;
    // Re-attach the legend if a shell re-render replaced the gutter subtree
    // (idempotent no-op once present).
    ensureLanesLegend(trackColumn);

    const track = TRACKS.find((t) => t.id === LANES_TRACK_ID);
    if (!track) return;

    // Batch the column measure once per frame (F3): the shell reads layout in
    // the same tick; here we read our own canvas host's width + scrollbar.
    const dpr = (typeof devicePixelRatio === "number" ? devicePixelRatio : 1) || 1;
    const pw = trackColumn.clientWidth;
    const scrollbarW = Math.max(0, trackColumn.offsetWidth - trackColumn.clientWidth);
    const geometry = trackGeometry(track, {
      pw,
      scrollbarW,
      viewStart: state.viewport.viewStart,
      viewEnd: state.viewport.viewEnd,
      dpr,
    });
    const drawW = geometry.time.drawW;
    if (drawW <= 0) return;

    // Own the canvas's DPR backing store (resize only on geometry change, F3).
    if (sizer === null || sizerCanvas !== canvas) {
      sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      sizerCanvas = canvas;
    }
    const ctx = sizer.ensure(drawW, track.height, dpr) as unknown as LaneDrawContext;

    const sel = state.selection;
    const selectedSpanIds = sel.spanFocus ? sel.spanFocus.chain : EMPTY_SET;
    const sharedMaxQ = sharedVisibleMaxQueue(
      data.workerIds,
      data.workerQueueSamples,
      state.viewport.viewStart,
      state.viewport.viewEnd,
    );

    const input: LanesRenderInput = {
      workerIds: data.workerIds,
      workerSpans: data.workerSpans,
      workerQueueSamples: data.workerQueueSamples,
      wakesByWorker: data.wakesByWorker,
      spansById: data.spansById,
      blockInPlaceGaps: state.trace.trace ? state.trace.trace.blockInPlaceGaps : [],
      hasCpuTime: data.hasCpuTime,
      hasSchedWait: data.hasSchedWait,
      viewStart: state.viewport.viewStart,
      viewEnd: state.viewport.viewEnd,
      selectedTaskId: sel.selectedTaskId,
      selectedSpanIds,
      hoveredWakerTaskId: sel.hoveredWakerTaskId,
      pinnedPoll: sel.pinnedEvent ? sel.pinnedEvent.poll : null,
      sharedMaxQ,
      dimmer,
    };
    renderLanes(ctx, input, { time: geometry.time, height: track.height });
  }

  // Only the slices the lanes actually read - NOT uiPrefs (legend chips filter
  // spans/events, not lanes). A uiPrefs-only change never redraws the lanes.
  const unsubscribe = store.subscribe(["trace", "viewport", "selection"], () => draw());

  // First paint if a trace is already resident; otherwise the subscription
  // fires when the trace loads.
  draw();

  return {
    refresh: () => store.update("viewport", {}),
    dispose(): void {
      unsubscribe();
      releaseClaim();
      disposeLegend();
    },
  };
}

const EMPTY_SET: ReadonlySet<string> = new Set<string>();

export { deriveLaneData } from "./data.js";
export type { LaneData } from "./data.js";
export { renderLanes, sharedVisibleMaxQueue } from "./render.js";
export type { LanesRenderInput, LanesLayout } from "./render.js";
export { resolveLaneClick } from "./click.js";
export type { LaneClickResult } from "./click.js";
export { assembleLaneHover } from "./hover.js";
export type { LaneHoverData } from "./hover.js";
export { LANES_LEGEND } from "./legend.js";
export type { LegendEntry } from "./legend.js";
