// src/pages/viewer/lane-interaction.ts - the DOM binding that wires the pure
// interaction machines (lib/interact/pointer, wheel, kb-selection) and the
// viewport actions to the viewer store, the track column, and the unified key
// router (T23; features/02 H1-H13, amendments K4 zoom-undo + K5 WASD).
//
// This is the ONLY place raw lane pointer/keyboard events are attached (the
// T22 lanes ship pure resolvers with NO listener; the T24 overlay owns HOVER).
// Every handler resolves geometry from a CLEAN layout read then dispatches
// store actions through the machines - it never renders (F2) and never scans
// O(allSpans) (F6: click resolution goes through the T22 resolveLaneClick /
// lib/trace/query helpers). Wheel zoom and pan frames update the viewport
// slice, which the store's RAF scheduler coalesces to <= 1 render/frame.
//
// SEAMS: a Shift-drag / keyboard region commit writes selection.sidebarRange -
// WHAT opens for that range (flamegraph / blocking / heap) is T32's. A lane
// click that lands on a poll with samples returns openStackFor - the Poll
// Detail surface is T31's; until it lands the selection still dispatches, the
// popup is a no-op noted below.

import { LABEL_W, timePanelLayout } from "../../lib/canvas/layout.js";
import type { TimePanelLayout } from "../../lib/canvas/layout.js";
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
import { mountSelectionOverlay } from "./selection-overlay.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";

/** Cursor-extension step as a fraction of the visible duration (H9, legacy 5%). */
const KB_STEP_FRACTION = 0.05;

const CONTROLS_CLASS = "d9-viewport-controls";

export interface LaneInteractionDeps {
  /** ARIA/toast announcer for selection + zoom-undo status (A16, via T21). */
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
 * Shift/Alt keyboard-selection listener, the H1-H4 viewport controls, and the
 * H10 selection overlay. `root` is the viewer app root (unused today; reserved
 * so the controls could relocate without touching the entry).
 */
export function mountLaneInteraction(
  root: HTMLElement,
  trackColumn: HTMLElement,
  store: ViewerStore,
  deps: LaneInteractionDeps,
): MountedLaneInteraction {
  void root;
  const pointer = createPointerMachine();
  const kbSel = createKbSelectionMachine();
  const viewport: ViewportActions = createViewportActions(store);
  const selectionOverlay = mountSelectionOverlay(trackColumn, store);

  // Frame-invariant lane data for click resolution, cached by the trace slice
  // (F5): recomputed only on load/reparse, not per click. Same derivation the
  // lanes track caches; clicks are rare, so a second cache is negligible.
  const laneData = store.derived(["trace"], (s): LaneData | null =>
    s.trace.trace ? deriveLaneData(s.trace.trace) : null,
  );

  // ── geometry (clean reads inside fresh events; F3-safe) ─────────────────

  function readColumnGeom(): ColumnGeom {
    const rect = trackColumn.getBoundingClientRect();
    const pw = trackColumn.clientWidth;
    const scrollbarW = Math.max(0, trackColumn.offsetWidth - trackColumn.clientWidth);
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

  /** Worker id under `clientY` on the lanes canvas, or null when off the lanes. */
  function workerAtClientY(clientY: number, workerIds: readonly number[]): number | null {
    const canvas = trackColumn.querySelector<HTMLElement>('canvas[data-track-canvas="lanes"]');
    const n = workerIds.length;
    if (canvas === null || n === 0) return null;
    const rect = canvas.getBoundingClientRect();
    if (clientY < rect.top || clientY >= rect.bottom || rect.height <= 0) return null;
    const idx = Math.floor(((clientY - rect.top) / rect.height) * n);
    return workerIds[idx] ?? null;
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
          // H7 seam: hand the range to the store; T32 opens the panel by data
          // present. This also retains the range (blocks kb-selection, H9).
          store.update("selection", { sidebarRange: { startNs: cmd.startNs, endNs: cmd.endNs } });
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
    if (e.button !== 0) return; // left button drives pan/select (H6)
    // Clicks on the viewport controls are their own (H4): never start a pan.
    if ((e.target as Element | null)?.closest?.(`.${CONTROLS_CLASS}`)) return;
    // A mousedown pre-empts an in-flight keyboard selection (legacy :5148).
    if (kbSel.active()) runCommands(kbSel.clear());
    const state = store.getState() as StoreState;
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
    if (pointer.phase() === "idle") return; // T24 owns the hover path
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
    // Record a completed pan-drag as ONE undo step (H6 -> K4 view history);
    // zoom/region commits already record inside viewport actions.
    if (wasPan && pointer.moved()) viewport.recordCurrent();
  }

  // ── click: task/span select (H section relies on drag discrimination) ────

  function onClick(e: MouseEvent): void {
    if (pointer.moved()) return; // the press was a drag, not a click
    if ((e.target as Element | null)?.closest?.(`.${CONTROLS_CLASS}`)) return;
    const data = laneData();
    if (data === null) return;
    // Restrict task/span selection to clicks over the lanes canvas (legacy
    // bounded the click to the lanes container, viewer.html:5353).
    const lanesCanvas = trackColumn.querySelector<HTMLElement>('canvas[data-track-canvas="lanes"]');
    if (lanesCanvas === null) return;
    const laneRect = lanesCanvas.getBoundingClientRect();
    if (e.clientY < laneRect.top || e.clientY > laneRect.bottom) return;

    const geom = readColumnGeom();
    const mouseXCol = e.clientX - geom.rectLeft;
    // Any lanes click clears the pinned custom-event marker (legacy :5363).
    if (mouseXCol < LABEL_W || mouseXCol > LABEL_W + geom.drawW) {
      store.update("selection", { selectedTaskId: null, pinnedEvent: null });
      return;
    }
    const ns = geom.layout.panelXToNs(mouseXCol);
    const workerId = workerAtClientY(e.clientY, data.workerIds);
    if (workerId === null) {
      store.update("selection", { selectedTaskId: null, pinnedEvent: null });
      return;
    }
    const polls = data.workerSpans[workerId]?.polls ?? [];
    const result = resolveLaneClick({
      workerId,
      ns,
      polls,
      allSpans: data.allSpans,
      spanById: data.spanByIdSingle,
      currentSelectedTaskId: store.getState().selection.selectedTaskId,
    });
    store.update("selection", {
      selectedTaskId: result.selectedTaskId,
      spanFocus: result.spanFocus,
      focusedSpanId: result.focusedSpanId,
      pinnedEvent: null,
    });
    // result.openStackFor -> Poll Detail (features/02 G15/R); T31's surface.
    // The selection is dispatched now; the popup lands when T31 wires it.
  }

  // ── wheel: Ctrl/Cmd = zoom-at-cursor (H5); plain = scroll lanes ──────────

  function onWheel(e: WheelEvent): void {
    const geom = readColumnGeom();
    const mouseXInDraw = e.clientX - geom.rectLeft - LABEL_W;
    const intent = wheelZoomIntent({
      ctrlKey: e.ctrlKey,
      metaKey: e.metaKey,
      deltaY: e.deltaY,
      mouseXInDraw,
      drawW: geom.drawW,
    });
    if (intent === null) return; // plain wheel: let the browser scroll the lanes
    if (store.getState().trace.trace === null) return;
    e.preventDefault();
    // Store dispatch only: N wheel notches in a frame -> N viewport updates ->
    // ONE coalesced render (03 F2 regression; the DoD's <= 1 render/frame).
    viewport.zoom(intent.factor, intent.centerFrac);
  }

  // ── Shift/Alt keyboard selection start (H9) ─────────────────────────────
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
    const state = store.getState() as StoreState;
    if (state.trace.trace === null) return;
    if (state.transient.drag !== null) return; // a drag owns the gesture
    // Blocked while the sidebar retains a range (legacy :6232).
    if (state.selection.sidebarRange !== null) return;
    e.preventDefault();
    const mode = e.key === "Shift" ? "region-select" : "zoom-select";
    runCommands(kbSel.start(mode, seedNs(state)));
  }

  // ── viewport controls (H1-H4): floating zoom in / out / fit panel ───────

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
      // Clicks here are not lane clicks (H4): stop them reaching the column.
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
    const trace = (store.getState() as StoreState).trace.trace;
    if (trace === null || trace === lastTrace) return;
    lastTrace = trace;
    // initViewportFromTrace (registered earlier) has fitted the viewport in
    // this same flush, so the current window is the baseline undo target.
    viewport.resetHistory();
    viewport.recordCurrent();
    ensureControls();
  });

  // Re-attach the controls after any shell re-render that could clobber the
  // foreign child (same defensiveness as the T24 overlay ensure). Cheap +
  // idempotent; runs after the shell (this mount is last).
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

  /** A zoom accelerator (H11/W-S): declines (falls through) without a trace. */
  function zoomKey(factor: number): () => boolean | void {
    return () => {
      if (!hasTrace()) return false;
      viewport.zoom(factor);
    };
  }

  /** A pan accelerator (A-D): declines (falls through) without a trace. */
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
    // Left/Right: extend the cursor during a selection (H9), else pan (H12).
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
    // Up/Down zoom in/out (H11). WASD mirror them + the arrow pans (K5).
    { key: "ArrowUp", onKey: zoomKey(0.5) },
    { key: "ArrowDown", onKey: zoomKey(2) },
    { key: "w", onKey: zoomKey(0.5) },
    { key: "s", onKey: zoomKey(2) },
    { key: "a", onKey: panKey(-1) },
    { key: "d", onKey: panKey(1) },
    // z: undo the last committed view (K4). Announced through A16.
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
