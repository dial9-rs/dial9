// The DOM binding that wires the pure interaction machines (pointer, wheel,
// kb-selection) and the viewport actions to the viewer store, the track
// column, and the unified key router.
//
// This is the ONLY place raw lane pointer/keyboard events are attached (the
// lanes ship pure resolvers with no listener; the overlay owns HOVER). Every
// handler resolves geometry from a CLEAN layout read then dispatches store
// actions through the machines - it never renders and never scans O(allSpans)
// (click resolution goes through resolveLaneClick / query helpers). Wheel zoom
// and pan frames update the viewport slice, which the store's RAF scheduler
// coalesces to <= 1 render/frame.
//
// A Shift-drag / keyboard region commit writes selection.sidebarRange; what
// opens for that range is the region panel's. A lane click on a poll with
// samples returns openStackFor, which drives the inspector's Poll Detail.

import {
  LABEL_W,
  headerAtLaneY,
  laneRowLayout,
  metricsLaneAtLaneY,
  timePanelLayout,
  workerAtLaneY,
} from "../../lib/canvas/layout.js";
import type { TimePanelLayout } from "../../lib/canvas/layout.js";
import { LANE_ROW_H, RUNTIME_HEADER_H } from "../../components/canvas/lanes/render.js";
import { lanesScrollbarWidth } from "../../lib/canvas/track-layout.js";
import { createPointerMachine } from "../../lib/interact/pointer.js";
import type { PointerCommand } from "../../lib/interact/pointer.js";
import { createKbSelectionMachine } from "../../lib/interact/kb-selection.js";
import type { KbSelectionCommand } from "../../lib/interact/kb-selection.js";
import { wheelZoomIntent } from "../../lib/interact/wheel.js";
import { isTextEntryTarget } from "../../lib/interact/keyboard.js";
import type { KeyBinding } from "../../lib/interact/keyboard.js";
import type { Announcer } from "../../lib/interact/announce.js";
import { deriveLaneData, resolveLaneClick } from "../../components/canvas/lanes/index.js";
import type { LaneData } from "../../components/canvas/lanes/index.js";
import { createViewportActions } from "./viewport-actions.js";
import type { ViewportActions } from "./viewport-actions.js";
import { toggleRuntimeCollapsed, toggleRuntimeMetricsCollapsed } from "./track-management.js";
import { mountSelectionOverlay } from "./selection-overlay.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";

/** Cursor-extension step as a fraction of the visible duration (5%). */
const KB_STEP_FRACTION = 0.05;

const CONTROLS_CLASS = "d9-viewport-controls";

export interface LaneInteractionDeps {
  /** ARIA/toast announcer for selection + zoom-undo status. */
  announcer: Announcer;
}

export interface MountedLaneInteraction {
  /**
   * Key bindings for the page router (registered by the entry BEFORE its
   * generic `?`/Escape fallbacks). Covers arrows/WASD/z/Enter and the
   * kb-selection Escape (which declines when no selection is active so the
   * entry's Escape cascade still runs).
   */
  keyBindings: readonly KeyBinding[];
  dispose(): void;
}

interface ColumnGeom {
  layout: TimePanelLayout;
  drawW: number;
  rectLeft: number;
}

/**
 * Mount the lane interaction layer against `store`, attaching pointer/wheel/
 * click listeners to `trackColumn` (and window for the drag move/up), the
 * Shift/Alt keyboard-selection listener, the viewport controls, and the
 * selection overlay.
 */
export function mountLaneInteraction(
  trackColumn: HTMLElement,
  store: ViewerStore,
  deps: LaneInteractionDeps,
): MountedLaneInteraction {
  const pointer = createPointerMachine();
  const kbSel = createKbSelectionMachine();
  const viewport: ViewportActions = createViewportActions(store);
  const selectionOverlay = mountSelectionOverlay(trackColumn, store);

  // Frame-invariant lane data for click resolution, cached by the trace slice:
  // recomputed only on load/reparse, not per click.
  const laneData = store.derived(["trace"], (s): LaneData | null =>
    s.trace.trace ? deriveLaneData(s.trace.trace) : null,
  );

  // ── geometry (clean reads inside fresh events) ──────────────────────────

  function readColumnGeom(): ColumnGeom {
    const rect = trackColumn.getBoundingClientRect();
    const pw = trackColumn.clientWidth;
    const scrollbarW = lanesScrollbarWidth(trackColumn);
    const { viewStart, viewEnd } = store.getState().viewport;
    return {
      layout: timePanelLayout({ pw, scrollbarW, viewStart, viewEnd }),
      drawW: pw - LABEL_W - scrollbarW,
      rectLeft: rect.left,
    };
  }

  /** Timestamp under `clientX`, clamped to the draw area (region/press edge). */
  function nsAtClientX(clientX: number, geom: ColumnGeom): number {
    const x = clientX - geom.rectLeft;
    const clamped = Math.max(LABEL_W, Math.min(LABEL_W + geom.drawW, x));
    return geom.layout.panelXToNs(clamped);
  }

  /** Worker id under `clientY` in the lanes box, or null when off the lanes /
   *  over a runtime header. Fixed-height rows + the box scrollTop, resolved
   *  through the shared row layout so click matches what the renderer drew. */
  function workerAtClientY(clientY: number, data: LaneData): number | null {
    const rowLayout = laneRowAt(clientY, data);
    return rowLayout === null ? null : workerAtLaneY(rowLayout.layout, rowLayout.localY);
  }

  /** Runtime-group name whose header band is under `clientY`, or null. A header
   *  click folds/unfolds that runtime rather than selecting a worker. */
  function runtimeHeaderAtClientY(clientY: number, data: LaneData): string | null {
    if (data.runtimeGroups.length <= 1) return null; // no headers to hit
    const rowLayout = laneRowAt(clientY, data);
    return rowLayout === null ? null : headerAtLaneY(rowLayout.layout, rowLayout.localY);
  }

  /** Runtime-group name whose SUMMARY lane is under `clientY`, or null. A click
   *  there folds/unfolds that lane's chart rather than selecting a worker. */
  function metricsLaneAtClientY(clientY: number, data: LaneData): string | null {
    if (data.metricsRuntimes.size === 0) return null; // no summary lanes to hit
    const rowLayout = laneRowAt(clientY, data);
    return rowLayout === null ? null : metricsLaneAtLaneY(rowLayout.layout, rowLayout.localY);
  }

  /** Shared lanes-box hit geometry: the collapse-aware row layout + the box-local
   *  y, or null when the point is off the lanes box. Built from the SAME collapse
   *  state the renderer drew from, so click resolves to what is on screen. */
  function laneRowAt(
    clientY: number,
    data: LaneData,
  ): { layout: ReturnType<typeof laneRowLayout>; localY: number } | null {
    const box = trackColumn.querySelector<HTMLElement>(".d9-lanes-viewport");
    if (box === null || data.workerIds.length === 0) return null;
    const rect = box.getBoundingClientRect();
    if (clientY < rect.top || clientY >= rect.bottom || rect.height <= 0) return null;
    const localY = clientY - rect.top + box.scrollTop;
    const prefs = store.getState().uiPrefs;
    const layout = laneRowLayout(
      data.runtimeGroups,
      LANE_ROW_H,
      RUNTIME_HEADER_H,
      prefs.collapsedRuntimes,
      { runtimes: data.metricsRuntimes, collapsed: prefs.collapsedRuntimeMetrics },
    );
    return { layout, localY };
  }

  // ── command executor (shared by both machines) ──────────────────────────

  function runCommands(cmds: readonly (PointerCommand | KbSelectionCommand)[]): void {
    for (const cmd of cmds) runCommand(cmd);
  }

  function runCommand(cmd: PointerCommand | KbSelectionCommand): void {
    switch (cmd.type) {
      case "set-drag":
        store.update("transient", { drag: cmd.drag });
        return;
      case "clear-drag":
        store.update("transient", { drag: null });
        return;
      case "pan":
        viewport.applyPan({ viewStart: cmd.viewStart, viewEnd: cmd.viewEnd });
        return;
      case "commit-region":
        if (cmd.kind === "zoom-select") {
          viewport.zoomToRegion(cmd.startNs, cmd.endNs);
        } else {
          // Hand the range to the store; the region panel opens by data
          // present. This also retains the range (blocks kb-selection).
          store.update("view", {
            regionMode: null,
            regionWorkerZoom: [],
            regionOffworkerZoom: [],
          });
          store.update("selection", {
            sidebarRange: { startNs: cmd.startNs, endNs: cmd.endNs },
            taskDump: null,
          });
        }
        return;
      case "cursor":
        trackColumn.ownerDocument.body.style.cursor = cmd.cursor;
        return;
      case "set-kb":
        store.update("transient", { keyboardSelection: cmd.selection });
        return;
      case "announce":
        deps.announcer.announce(cmd.message);
        return;
    }
  }

  // ── pointer: mousedown (column) + mousemove/mouseup (window) ─────────────

  function onMouseDown(e: MouseEvent): void {
    if (e.button !== 0) return; // left button drives pan/select
    // Clicks on the viewport controls are their own: never start a pan.
    if ((e.target as Element | null)?.closest?.(`.${CONTROLS_CLASS}`)) return;
    // The track-management strip (collapse caret + reorder grip) owns its own
    // gestures: the grip is a native drag source (draggable), and native DnD
    // swallows the matching mouseup, so a pan started here would never be
    // released and would keep panning after the drop. Never start a pan on it.
    if ((e.target as Element | null)?.closest?.(".d9-track-manage-strip")) return;
    // The lanes resize gutter owns its own drag (the lanes mount): never pan.
    if ((e.target as Element | null)?.closest?.(".d9-lanes-resize")) return;
    // A mousedown pre-empts an in-flight keyboard selection.
    if (kbSel.active()) runCommands(kbSel.clear());
    const state = store.getState();
    const vp = state.viewport;
    const geom = readColumnGeom();
    runCommands(
      pointer.down({
        clientX: e.clientX,
        shiftKey: e.shiftKey,
        altKey: e.altKey,
        nsAt: nsAtClientX(e.clientX, geom),
        viewStart: vp.viewStart,
        viewEnd: vp.viewEnd,
        minTs: vp.minTs,
        maxTs: vp.maxTs,
        hasTrace: state.trace.trace !== null,
      }),
    );
  }

  function onMouseMove(e: MouseEvent): void {
    if (pointer.phase() === "idle") return; // the overlay owns the hover path
    const geom = readColumnGeom();
    runCommands(
      pointer.move({
        clientX: e.clientX,
        nsAtClamped: nsAtClientX(e.clientX, geom),
        drawW: geom.drawW,
      }),
    );
  }

  function onMouseUp(): void {
    if (pointer.phase() === "idle") return;
    const wasPan = pointer.phase() === "pan";
    runCommands(pointer.up());
    // Record a completed pan-drag as ONE undo step; zoom/region commits already
    // record inside viewport actions.
    if (wasPan && pointer.moved()) viewport.recordCurrent();
  }

  // ── click: task/span select (relies on drag discrimination) ────

  function onClick(e: MouseEvent): void {
    if (pointer.moved()) return; // the press was a drag, not a click
    if ((e.target as Element | null)?.closest?.(`.${CONTROLS_CLASS}`)) return;
    const data = laneData();
    if (data === null) return;
    // Restrict task/span selection to clicks over the lanes box.
    const box = trackColumn.querySelector<HTMLElement>(".d9-lanes-viewport");
    if (box === null) return;
    const laneRect = box.getBoundingClientRect();
    if (e.clientY < laneRect.top || e.clientY > laneRect.bottom) return;

    const geom = readColumnGeom();
    const mouseXCol = e.clientX - geom.rectLeft;
    // Any lanes click clears the pinned custom-event marker.
    if (mouseXCol < LABEL_W || mouseXCol > LABEL_W + geom.drawW) {
      store.update("selection", {
        selectedTaskId: null,
        pinnedEvent: null,
        pollDetail: null,
        taskDump: null,
      });
      return;
    }
    // A click on a runtime header band folds/unfolds that runtime - never a
    // worker select and never a selection clear.
    const headerName = runtimeHeaderAtClientY(e.clientY, data);
    if (headerName !== null) {
      toggleRuntimeCollapsed(store, headerName);
      return;
    }
    // A click on a runtime's SUMMARY lane folds/unfolds its chart - likewise
    // never a worker select and never a selection clear.
    const metricsName = metricsLaneAtClientY(e.clientY, data);
    if (metricsName !== null) {
      toggleRuntimeMetricsCollapsed(store, metricsName);
      return;
    }
    const ns = geom.layout.panelXToNs(mouseXCol);
    const workerId = workerAtClientY(e.clientY, data);
    if (workerId === null) {
      store.update("selection", {
        selectedTaskId: null,
        pinnedEvent: null,
        pollDetail: null,
        taskDump: null,
      });
      return;
    }
    const polls = data.workerSpans[workerId]?.polls ?? [];
    const result = resolveLaneClick({
      workerId,
      ns,
      polls,
      allSpans: data.allSpans,
      columnarSpans: data.columnarSpans,
      spanById: data.spanByIdSingle,
      currentSelectedTaskId: store.getState().selection.selectedTaskId,
    });
    store.update("selection", {
      selectedTaskId: result.selectedTaskId,
      spanFocus: result.spanFocus,
      focusedSpanId: result.focusedSpanId,
      pinnedEvent: null,
      // A click on a poll carrying CPU/sched samples opens Poll Detail in the
      // inspector; a click on a bare poll (or empty space) clears any open Poll
      // Detail. `openStackFor` is null in both no-sample cases, so this
      // dispatch doubles as the close.
      pollDetail: result.openStackFor,
      taskDump: null,
    });
  }

  // ── wheel: Ctrl/Cmd = zoom-at-cursor; plain = scroll lanes ───────────────

  function onWheel(e: WheelEvent): void {
    // Plain wheel scrolls the lanes: bail BEFORE any layout read so a scroll
    // never forces a measure. Only Ctrl/Cmd wheels zoom.
    if (!e.ctrlKey && !e.metaKey) return;
    if (store.getState().trace.trace === null) return;
    const geom = readColumnGeom();
    const mouseXInDraw = e.clientX - geom.rectLeft - LABEL_W;
    const intent = wheelZoomIntent({
      ctrlKey: e.ctrlKey,
      metaKey: e.metaKey,
      deltaY: e.deltaY,
      mouseXInDraw,
      drawW: geom.drawW,
    });
    if (intent === null) return;
    e.preventDefault();
    // Store dispatch only: N wheel notches in a frame -> N viewport updates ->
    // ONE coalesced render.
    viewport.zoom(intent.factor, intent.centerFrac);
  }

  // ── Shift/Alt keyboard selection start ──────────────────────────────────
  //
  // A dedicated window listener, NOT a router binding: the unified router
  // drops Alt-modified events (its chord gate), so it can never deliver a bare
  // Alt press. This owns BOTH Shift and Alt so the two behave symmetrically.

  function seedNs(state: StoreState): number {
    const m = state.transient.mouseNs;
    const { viewStart, viewEnd } = state.viewport;
    if (m !== null && m >= viewStart && m <= viewEnd) return m;
    return (viewStart + viewEnd) / 2;
  }

  function onModifierKeyDown(e: KeyboardEvent): void {
    if (e.key !== "Shift" && e.key !== "Alt") return;
    if (e.repeat) return; // a held modifier must not auto-confirm the selection
    if (isTextEntryTarget(e.target)) return; // typing in a filter field
    const state = store.getState();
    if (state.trace.trace === null) return;
    if (state.transient.drag !== null) return; // a drag owns the gesture
    // Blocked while the sidebar retains a range.
    if (state.selection.sidebarRange !== null) return;
    e.preventDefault();
    const mode = e.key === "Shift" ? "region-select" : "zoom-select";
    runCommands(kbSel.start(mode, seedNs(state)));
  }

  // ── viewport controls: floating zoom in / out / fit panel ───────

  function ensureControls(): void {
    if (trackColumn.querySelector(`.${CONTROLS_CLASS}`) !== null) return;
    const doc = trackColumn.ownerDocument;
    const panel = doc.createElement("div");
    panel.className = CONTROLS_CLASS;
    panel.setAttribute("role", "group");
    panel.setAttribute("aria-label", "Viewport controls");
    const mk = (id: string, label: string, title: string, onClick: () => void): HTMLButtonElement => {
      const b = doc.createElement("button");
      b.type = "button";
      b.id = id;
      b.textContent = label;
      b.title = title;
      b.setAttribute("aria-label", title);
      // Clicks here are not lane clicks: stop them reaching the column.
      b.addEventListener("click", (ev) => {
        ev.stopPropagation();
        onClick();
      });
      return b;
    };
    panel.appendChild(mk("btn-zoom-in", "+", "Zoom in", () => viewport.zoom(0.5)));
    panel.appendChild(mk("btn-zoom-out", "-", "Zoom out", () => viewport.zoom(2)));
    panel.appendChild(mk("btn-fit", "Fit", "Fit all", () => viewport.fit()));
    trackColumn.appendChild(panel);
  }

  // ── zoom-history baseline: reset + seed on each newly loaded trace ───────

  let lastTrace: StoreState["trace"]["trace"] | null = null;
  const unsubTrace = store.subscribe(["trace"], () => {
    const trace = store.getState().trace.trace;
    if (trace === null || trace === lastTrace) return;
    lastTrace = trace;
    // The explicit loaded-trace transition commits the fitted/restored
    // viewport synchronously before this notification flush, so subscriber
    // registration order cannot change the baseline.
    viewport.resetHistory();
    viewport.recordCurrent();
    ensureControls();
  });

  // Re-attach the controls after any shell re-render that could clobber the
  // foreign child. Cheap + idempotent; runs after the shell (this mount is
  // last).
  const unsubChrome = store.subscribe(["trace", "viewport", "selection", "uiPrefs"], () => {
    if (store.getState().trace.trace !== null) ensureControls();
  });

  // ── keyboard bindings for the unified router (arrows / WASD / z) ─────────

  function hasTrace(): boolean {
    return store.getState().trace.trace !== null;
  }

  function extendSelection(dir: -1 | 1): void {
    const vp = store.getState().viewport;
    const step = (vp.viewEnd - vp.viewStart) * KB_STEP_FRACTION;
    runCommands(kbSel.extend(dir, step, vp.minTs, vp.maxTs));
  }

  /** A zoom accelerator: declines (falls through) without a trace. */
  function zoomKey(factor: number): () => boolean | void {
    return () => {
      if (!hasTrace()) return false;
      viewport.zoom(factor);
    };
  }

  /** A pan accelerator: declines (falls through) without a trace. */
  function panKey(dir: -1 | 1): () => boolean | void {
    return () => {
      if (!hasTrace()) return false;
      viewport.pan(dir);
    };
  }

  const keyBindings: KeyBinding[] = [
    // Escape: cancel an active kb-selection first; decline otherwise so the
    // entry's Escape cascade (help/popups/selection clear) still runs.
    {
      key: "Escape",
      onKey: () => {
        if (!kbSel.active()) return false;
        runCommands(kbSel.cancel());
        return true;
      },
    },
    // Enter confirms an active kb-selection; declines otherwise.
    {
      key: "Enter",
      onKey: () => {
        if (!kbSel.active()) return false;
        runCommands(kbSel.confirm());
        return true;
      },
    },
    // Left/Right: extend the cursor during a selection, else pan.
    {
      key: "ArrowLeft",
      onKey: () => {
        if (kbSel.active()) {
          extendSelection(-1);
          return;
        }
        if (!hasTrace()) return false;
        viewport.pan(-1);
      },
    },
    {
      key: "ArrowRight",
      onKey: () => {
        if (kbSel.active()) {
          extendSelection(1);
          return;
        }
        if (!hasTrace()) return false;
        viewport.pan(1);
      },
    },
    // Up/Down zoom in/out. WASD mirror them + the arrow pans.
    { key: "ArrowUp", onKey: zoomKey(0.5) },
    { key: "ArrowDown", onKey: zoomKey(2) },
    { key: "w", onKey: zoomKey(0.5) },
    { key: "s", onKey: zoomKey(2) },
    { key: "a", onKey: panKey(-1) },
    { key: "d", onKey: panKey(1) },
    // f: fit the whole trace in view (the action the "Fit all" button drives).
    // Commits to history so `z` can undo it.
    {
      key: "f",
      onKey: () => {
        if (!hasTrace()) return false;
        viewport.fit();
        deps.announcer.announce("Fitted the whole trace in view");
      },
    },
    // z: undo the last committed view. Announced through the announcer.
    {
      key: "z",
      onKey: () => {
        if (!hasTrace()) return false;
        deps.announcer.announce(viewport.undo() ? "Zoom undone" : "Nothing to undo");
      },
    },
    // `?`: cancel any active selection, then decline so the entry toggles help.
    {
      key: "?",
      onKey: () => {
        if (kbSel.active()) runCommands(kbSel.clear());
        return false;
      },
    },
  ];

  // ── attach ───────────────────────────────────────────────────────────────

  const doc = trackColumn.ownerDocument;
  const win = doc.defaultView ?? window;
  trackColumn.addEventListener("mousedown", onMouseDown);
  trackColumn.addEventListener("click", onClick);
  trackColumn.addEventListener("wheel", onWheel, { passive: false });
  win.addEventListener("mousemove", onMouseMove);
  win.addEventListener("mouseup", onMouseUp);
  win.addEventListener("keydown", onModifierKeyDown);

  // First controls paint if a trace is already resident.
  if (store.getState().trace.trace !== null) {
    lastTrace = store.getState().trace.trace;
    viewport.recordCurrent();
    ensureControls();
  }

  return {
    keyBindings,
    dispose(): void {
      trackColumn.removeEventListener("mousedown", onMouseDown);
      trackColumn.removeEventListener("click", onClick);
      trackColumn.removeEventListener("wheel", onWheel);
      win.removeEventListener("mousemove", onMouseMove);
      win.removeEventListener("mouseup", onMouseUp);
      win.removeEventListener("keydown", onModifierKeyDown);
      unsubTrace();
      unsubChrome();
      selectionOverlay.dispose();
      trackColumn.querySelector<HTMLElement>(`.${CONTROLS_CLASS}`)?.remove();
    },
  };
}
