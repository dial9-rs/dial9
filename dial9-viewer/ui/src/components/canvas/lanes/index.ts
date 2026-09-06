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
import { renderLaneLabels } from "./labels.js";
import { LABEL_W } from "../../../lib/canvas/layout.js";

const LANES_TRACK_ID: TrackId = "lanes";

/** Drag-resize clamp bounds for the lanes box (CSS px). */
const MIN_LANES_H = 80;
const MAX_LANES_VH = 0.7;

function clampLanesHeight(h: number): number {
  const maxH = typeof window !== "undefined" ? window.innerHeight * MAX_LANES_VH : Infinity;
  return Math.round(Math.max(MIN_LANES_H, Math.min(maxH, h)));
}

export interface MountedLanes {
  /**
   * Scroll the lanes box so `workerId`'s row is visible, returning true when it
   * actually moved. The lanes box scrolls independently of the time axis, so
   * navigating the viewport to a point of interest does not by itself put the
   * relevant lane on screen.
   */
  revealWorker(workerId: number): boolean;
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
  // The label gutter's own sizer: a second canvas, same DPR discipline.
  let labelSizer: CanvasSizer<CanvasRenderingContext2D> | null = null;
  let labelSizerCanvas: HTMLCanvasElement | null = null;

  function laneCanvas(): HTMLCanvasElement | null {
    return trackColumn.querySelector<HTMLCanvasElement>(
      `canvas[data-track-canvas="${LANES_TRACK_ID}"]`,
    );
  }

  function labelCanvas(): HTMLCanvasElement | null {
    return trackColumn.querySelector<HTMLCanvasElement>(".d9-lanes-label-canvas");
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

    // Fixed-height runtime-aware stack (headers only when >1 runtime group);
    // a folded runtime keeps its header but drops its worker rows.
    const rowLayout = laneRowLayout(
      data.runtimeGroups,
      LANE_ROW_H,
      RUNTIME_HEADER_H,
      state.uiPrefs.collapsedRuntimes,
      {
        runtimes: data.metricsRuntimes,
        collapsed: state.uiPrefs.collapsedRuntimeMetrics,
      },
    );
    // Size the inner spacer so the box scrolls exactly the overflow past the
    // window (the sticky canvas occupies the first `viewportH` of flow).
    setSpacerHeight(box, Math.max(0, rowLayout.contentHeight - viewportH));
    if (Math.abs(box.scrollTop - state.uiPrefs.lanesScrollTop) >= 1) {
      box.scrollTop = state.uiPrefs.lanesScrollTop;
    }
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
      runtimeMetrics: data.runtimeMetrics,
      runtimeTaskSpawns: data.runtimeTaskSpawns.byRuntime,
      laneIdentity: data.laneIdentity,
      runtimeAccents: data.runtimeAccents,
      workerQueueSamples: data.workerQueueSamples,
      wakesByWorker: data.wakesByWorker,
      spansById: data.spansById,
      blockInPlaceGaps: state.trace.trace ? state.trace.trace.blockInPlaceGaps : [],
      hasCpuTime: data.hasCpuTime,
      hasSchedWait: data.hasSchedWait,
      hasLocalQueueDepth: data.hasLocalQueueDepth,
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

    // The label gutter, painted from the SAME rowLayout + scrollTop in the SAME
    // frame, so a row's name can never drift from the row it names.
    drawLabels(rowLayout, scrollTop, viewportH, dpr, data);
  }

  /**
   * Paint the per-row label gutter. Sized to the VISIBLE box height (the gutter
   * clips it), matching the virtualized lanes canvas: both cull to the same
   * scroll window from the same row stack.
   *
   * The gutter is outside the scrolling box, so it needs no sticky positioning —
   * repainting it from `scrollTop` is what keeps it in step, which is exactly how
   * the lanes canvas already works.
   */
  function drawLabels(
    rowLayout: LaneRowLayout,
    scrollTop: number,
    viewportH: number,
    dpr: number,
    data: LaneData,
  ): void {
    const canvas = labelCanvas();
    if (!canvas) return;
    if (labelSizer === null || labelSizerCanvas !== canvas) {
      labelSizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      labelSizerCanvas = canvas;
    }
    // The gutter canvas spans the full box height: its row 0 must start at the
    // same y as the lanes canvas's row 0, so the gutter carries no padding and no
    // leading track-name line (CSS makes that name sr-only). Any inset here would
    // shift every label relative to the lane it names.
    const labelH = viewportH;
    const ctx = labelSizer.ensure(LABEL_W, labelH, dpr);
    renderLaneLabels(ctx, {
      rowLayout,
      scrollTop,
      labelW: LABEL_W,
      height: labelH,
      runtimeAccents: data.runtimeAccents,
      workerRuntime: data.workerRuntime,
    });
  }

  // ── Inner scroll: redraw the visible row window on box scroll ─────────────

  let scrolledBox: HTMLElement | null = null;
  let scrollRaf = 0;
  const onBoxScroll = (): void => {
    const top = scrolledBox?.scrollTop ?? 0;
    if (top !== store.getState().uiPrefs.lanesScrollTop) {
      store.update("uiPrefs", { lanesScrollTop: top });
    }
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

  // The lanes read `trace`/`viewport`/`selection`, plus `uiPrefs` for the
  // per-runtime fold state (a header click toggles collapsedRuntimes, which must
  // repaint the lanes). Legend-chip uiPrefs changes also poke a redraw, but that
  // is one cheap virtualized-canvas paint, not the span/cpu/queue panels.
  const unsubscribe = store.subscribe(["trace", "viewport", "selection", "uiPrefs"], () => draw());

  // First paint if a trace is already resident; otherwise the subscription
  // fires when the trace loads.
  draw();

  return {
    /**
     * Scroll the lanes box the minimum needed to bring `workerId`'s row into
     * view, and report whether it moved.
     *
     * The lanes box is its own scroll container, so navigating the horizontal
     * viewport to a point of interest still leaves it off-screen when it lives
     * on, say, worker 28 of 32 - the timeline jumps, the user sees nothing
     * change, and the jump reads as broken. Minimal scrolling (rather than
     * centring) keeps the surrounding workers stable when stepping n/p through
     * issues on nearby lanes.
     */
    revealWorker(workerId: number): boolean {
      const data = laneData();
      const box = lanesBox();
      if (!data || !box) return false;
      // Auto-expand the runtime that owns this worker when it is folded, so
      // stepping n/p to an issue on a collapsed runtime still brings its lane on
      // screen instead of silently failing to scroll.
      const collapsed = store.getState().uiPrefs.collapsedRuntimes;
      const owning = data.runtimeGroups.find((g) => g.workerIds.includes(workerId));
      let effective = collapsed;
      if (owning && collapsed[owning.name] === true) {
        effective = { ...collapsed, [owning.name]: false };
        store.update("uiPrefs", { collapsedRuntimes: effective });
      }
      const { rows } = laneRowLayout(
        data.runtimeGroups,
        LANE_ROW_H,
        RUNTIME_HEADER_H,
        effective,
        {
          runtimes: data.metricsRuntimes,
          collapsed: store.getState().uiPrefs.collapsedRuntimeMetrics,
        },
      );
      const row = rows.find((r) => r.kind === "worker" && r.workerId === workerId);
      if (row === undefined) return false;

      const viewportH = box.clientHeight;
      if (viewportH <= 0) return false;
      const top = box.scrollTop;
      const bottom = top + viewportH;
      let next = top;
      if (row.y < top) next = row.y;
      else if (row.y + row.height > bottom) next = row.y + row.height - viewportH;
      if (next === top) return false;

      box.scrollTop = next;
      store.update("uiPrefs", { lanesScrollTop: next });
      // The canvas is sticky and painted from scrollTop, so a programmatic
      // scroll has to repaint exactly as a user scroll does.
      draw();
      return true;
    },
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
export { buildLaneIdentities, runtimeAccent, RUNTIME_ACCENTS } from "./chrome.js";
export type { LaneIdentities, LaneIdentity } from "./chrome.js";
export { resolveLaneClick } from "./click.js";
export type { LaneClickResult } from "./click.js";
export { assembleLaneHover } from "./hover.js";
export type { LaneHoverData } from "./hover.js";
export { LANES_LEGEND } from "./legend.js";
export type { LegendEntry } from "./legend.js";
