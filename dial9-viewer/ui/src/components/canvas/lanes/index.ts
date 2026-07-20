// Mount the worker-lanes track.
//
// Wires render.ts to the viewer store: subscribes to the slices the lanes
// depend on (trace / viewport / selection) and redraws ONLY the lanes canvas
// when one of them changes (a selection change redraws lanes, not the
// span/cpu/queue panels, which own their own subscriptions). Frame-invariant
// lane data (buildWorkerSpans etc.) is computed through the store's derived()
// cache keyed by the `trace` slice, so pan and selection never re-derive it.
//
// The lanes canvas lives in the shell's track column. This mount claims the
// "lanes" track (track-renderers.ts) so the shell stops painting the
// placeholder over it, then owns the canvas's DPR sizing + draw.

import type { ViewerStore } from "../../../store/store.js";
import { createCanvasSizer } from "../../../lib/canvas/dpr.js";
import type { CanvasSizer } from "../../../lib/canvas/dpr.js";
import { makeColorDimmer } from "../../../lib/canvas/palette.js";
import { laneRowLayout } from "../../../lib/canvas/layout.js";
import type { LaneRowLayout } from "../../../lib/canvas/layout.js";
import {
  TRACKS,
  lanesScrollbarWidth,
  trackGeometry,
} from "../../../lib/canvas/track-layout.js";
import type { TrackId } from "../../../lib/canvas/track-layout.js";
import { claimTrack } from "../../../lib/canvas/track-renderers.js";
import { deriveLaneData } from "./data.js";
import type { LaneData } from "./data.js";
import {
  LANE_ROW_H,
  RUNTIME_HEADER_H,
  type LanesRenderInput,
  renderLanes,
  sharedVisibleMaxQueue,
} from "./render.js";
import { ensureLanesLegend, mountLanesLegend } from "./legend.js";

const LANES_TRACK_ID: TrackId = "lanes";

/** Drag-resize clamp bounds for the lanes box (CSS px). */
const MIN_LANES_H = 80;
const MAX_LANES_VH = 0.7;

function clampLanesHeight(h: number): number {
  const maxH = typeof window !== "undefined" ? window.innerHeight * MAX_LANES_VH : Infinity;
  return Math.round(Math.max(MIN_LANES_H, Math.min(maxH, h)));
}

export interface MountedLanes {
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
  // replaced. Non-null only once a trace has loaded.
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

  function lanesBox(): HTMLElement | null {
    return trackColumn.querySelector<HTMLElement>(".d9-lanes-viewport");
  }

  function draw(): void {
    const state = store.getState();
    const data = laneData();
    if (!data || data.workerIds.length === 0) return;
    const canvas = laneCanvas();
    const box = lanesBox();
    if (!canvas || !box) return;
    // Re-attach the legend if a shell re-render replaced the gutter subtree
    // (idempotent no-op once present).
    ensureLanesLegend(trackColumn);
    ensureScrollListener(box);

    const track = TRACKS.find((t) => t.id === LANES_TRACK_ID);
    if (!track) return;

    // Batch the column measure once per frame: the shell reads layout in the
    // same tick; here we read our own canvas host's width + the LANES-BOX
    // scrollbar (the gutter every track reserves so axes align).
    const dpr = (typeof devicePixelRatio === "number" ? devicePixelRatio : 1) || 1;
    const pw = trackColumn.clientWidth;
    const scrollbarW = lanesScrollbarWidth(trackColumn);
    const geometry = trackGeometry(track, {
      pw,
      scrollbarW,
      viewStart: state.viewport.viewStart,
      viewEnd: state.viewport.viewEnd,
      dpr,
    });
    const drawW = geometry.time.drawW;
    if (drawW <= 0) return;

    // The visible box height (the resizeable window); the canvas is sized to it,
    // NOT to the full stacked content - the content scrolls behind it.
    const viewportH = box.clientHeight;
    if (viewportH <= 0) return;

    // Fixed-height runtime-aware stack (headers only when >1 runtime group).
    const rowLayout = laneRowLayout(data.runtimeGroups, LANE_ROW_H, RUNTIME_HEADER_H);
    // Size the inner spacer so the box scrolls exactly the overflow past the
    // window (the sticky canvas occupies the first `viewportH` of flow).
    setSpacerHeight(box, Math.max(0, rowLayout.contentHeight - viewportH));
    const scrollTop = box.scrollTop;

    // Own the canvas's DPR backing store (resize only on geometry change).
    if (sizer === null || sizerCanvas !== canvas) {
      sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      sizerCanvas = canvas;
    }
    const ctx = sizer.ensure(drawW, viewportH, dpr);

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
    renderLanes(ctx, input, {
      time: geometry.time,
      height: viewportH,
      rowLayout,
      scrollTop,
    });
  }

  // ── Inner scroll: redraw the visible row window on box scroll ─────────────

  let scrolledBox: HTMLElement | null = null;
  let scrollRaf = 0;
  const onBoxScroll = (): void => {
    if (scrollRaf) return;
    scrollRaf = requestAnimationFrame(() => {
      scrollRaf = 0;
      draw();
    });
  };
  function ensureScrollListener(box: HTMLElement): void {
    if (scrolledBox === box) return;
    scrolledBox?.removeEventListener("scroll", onBoxScroll);
    box.addEventListener("scroll", onBoxScroll, { passive: true });
    scrolledBox = box;
  }
  function setSpacerHeight(box: HTMLElement, h: number): void {
    const spacer = box.querySelector<HTMLElement>(".d9-lanes-spacer");
    if (spacer) spacer.style.height = `${h}px`;
  }

  // ── Drag-resize the box (mirrors the inspector width resize) ──────────────

  let dragging = false;
  let pendingH = 0;
  function boxTop(): number {
    const box = lanesBox();
    return box ? box.getBoundingClientRect().top : 0;
  }
  function onResizeDown(e: MouseEvent): void {
    e.preventDefault();
    dragging = true;
    trackColumn.ownerDocument.body.style.userSelect = "none";
    trackColumn.ownerDocument.body.style.cursor = "row-resize";
    win.addEventListener("mousemove", onResizeMove);
    win.addEventListener("mouseup", onResizeUp);
  }
  function onResizeMove(e: MouseEvent): void {
    if (!dragging) return;
    pendingH = clampLanesHeight(e.clientY - boxTop());
    // Live: size the box + redraw ONLY the lanes canvas (draw() reads the live
    // box.clientHeight). Deliberately not through the store - a shell re-render
    // would reset the box height from the not-yet-updated viewmodel and fight
    // this. The store (and its trackPrefs persistence) commit once on mouseup.
    const box = lanesBox();
    if (box) box.style.height = `${pendingH}px`;
    draw();
  }
  function onResizeUp(): void {
    if (!dragging) return;
    dragging = false;
    trackColumn.ownerDocument.body.style.userSelect = "";
    trackColumn.ownerDocument.body.style.cursor = "";
    win.removeEventListener("mousemove", onResizeMove);
    win.removeEventListener("mouseup", onResizeUp);
    // Commit once: the viewmodel now matches the box, and the uiPrefs ->
    // trackPrefs subscriber persists the final height.
    if (pendingH > 0 && pendingH !== store.getState().uiPrefs.lanesViewportHeight) {
      store.update("uiPrefs", { lanesViewportHeight: pendingH });
    }
  }
  const doc = trackColumn.ownerDocument;
  const win = doc.defaultView ?? window;
  const onColumnDown = (e: MouseEvent): void => {
    if ((e.target as Element | null)?.closest?.(".d9-lanes-resize")) onResizeDown(e);
  };
  trackColumn.addEventListener("mousedown", onColumnDown);

  // Only the slices the lanes actually read - NOT uiPrefs (legend chips filter
  // spans/events, not lanes). A uiPrefs-only change never redraws the lanes; the
  // resize poke goes through the viewport channel.
  const unsubscribe = store.subscribe(["trace", "viewport", "selection"], () => draw());

  // First paint if a trace is already resident; otherwise the subscription
  // fires when the trace loads.
  draw();

  return {
    dispose(): void {
      unsubscribe();
      releaseClaim();
      disposeLegend();
      scrolledBox?.removeEventListener("scroll", onBoxScroll);
      if (scrollRaf) cancelAnimationFrame(scrollRaf);
      trackColumn.removeEventListener("mousedown", onColumnDown);
      win.removeEventListener("mousemove", onResizeMove);
      win.removeEventListener("mouseup", onResizeUp);
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
