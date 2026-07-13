// lib/interact/pointer.ts - the lane pointer gesture state machine (T23;
// docs/ui-inventory/02-architecture.md 2.5, features/02 H5-H8/H10).
//
// The single owner of the drag-vs-click discrimination that viewer.html
// interleaved across three handlers (mousedown :5145, mousemove :5271,
// mouseup :5303). A PURE, DOM-free machine: it consumes abstract pointer
// events (client x + modifiers + the timestamp under the pointer, resolved by
// the caller through the shared layout) and returns COMMANDS the DOM binding
// (pages/viewer/lane-interaction.ts) executes as store actions. It never
// touches the store, the DOM, or renders (F2) - which makes every transition,
// including the cancel paths, unit-testable under plain Node.
//
// Gestures (legacy thresholds preserved, viewer.html):
//   - plain drag  -> PAN the viewport (H6), intent at 3px of movement;
//   - Shift+drag  -> REGION select (H7), the gesture only; WHAT the region
//     opens (flamegraph / blocking / heap) is T32's - this machine emits a
//     `commit-region` command carrying [start,end], the binding routes it to
//     the store (selection.sidebarRange), T32 reacts;
//   - Alt+drag    -> ZOOM select (H8): commit-region with kind "zoom", the
//     binding zooms the viewport to the range;
//   - a press+release under 3px is NOT a drag: the machine reports it as
//     unmoved so the binding's click handler runs the task/span select.
//
// Shift takes precedence over Alt (matching legacy `altZooming = altKey &&
// !shiftKey`): Shift+Alt+drag is a region select.

/** Movement (CSS px) before a press becomes a drag (H6/H7 intent threshold). */
export const DRAG_INTENT_PX = 3;

/** Which drag gesture a press initiated. */
export type GestureKind = "pan" | "region-select" | "zoom-select";

/** Inputs at pointer-down, resolved by the binding from the raw event. */
export interface PointerDownInput {
  /** Pointer clientX (CSS px) - the anchor for the 3px intent threshold. */
  clientX: number;
  shiftKey: boolean;
  altKey: boolean;
  /** Timestamp under the pointer at press (trace ns; layout-resolved, clamped). */
  nsAt: number;
  /** Current visible window at press (the pan anchor). */
  viewStart: number;
  viewEnd: number;
  /** Navigable bounds (pan/zoom clamp). */
  minTs: number;
  maxTs: number;
  /** No gesture starts without a trace (legacy required `trace`). */
  hasTrace: boolean;
}

/** Inputs on each pointer move (binding resolves ns through the layout). */
export interface PointerMoveInput {
  /** Pointer clientX (CSS px) - drives the intent threshold and pan delta. */
  clientX: number;
  /** Timestamp under the pointer NOW, clamped to the draw area (region edge). */
  nsAtClamped: number;
  /** Draw-area width (CSS px) for the pan ns/px conversion. */
  drawW: number;
}

/** The in-store drag snapshot the binding writes to `transient.drag`. */
export interface DragSnapshot {
  kind: GestureKind;
  startX: number;
  startNs: number;
  curNs: number;
  moved: boolean;
}

/**
 * Commands the machine emits; the binding maps each to a store action.
 *   - set-drag:      write `transient.drag` (starts/updates the drag snapshot);
 *   - clear-drag:    write `transient.drag = null` (gesture ended/cancelled);
 *   - pan:           apply the panned window (H6 frame; the binding coalesces);
 *   - commit-region: a completed region/zoom selection [startNs,endNs];
 *   - cursor:        the document cursor to show ("grabbing"/"crosshair"/"").
 */
export type PointerCommand =
  | { type: "set-drag"; drag: DragSnapshot }
  | { type: "clear-drag" }
  | { type: "pan"; viewStart: number; viewEnd: number }
  | { type: "commit-region"; kind: "region-select" | "zoom-select"; startNs: number; endNs: number }
  | { type: "cursor"; cursor: string };

type MachineState =
  | { phase: "idle" }
  | {
      phase: "pan";
      startX: number;
      anchorStart: number;
      anchorEnd: number;
      minTs: number;
      maxTs: number;
    }
  | {
      phase: "select";
      kind: "region-select" | "zoom-select";
      startX: number;
      startNs: number;
      curNs: number;
    };

export interface PointerMachine {
  /** Begin a gesture (mousedown). */
  down(input: PointerDownInput): PointerCommand[];
  /** Advance the active gesture (mousemove). */
  move(input: PointerMoveInput): PointerCommand[];
  /** Finish the active gesture (mouseup). */
  up(): PointerCommand[];
  /** Abort any active gesture (Escape, keyboard-selection pre-empt, blur). */
  cancel(): PointerCommand[];
  /** Current phase (for tests). */
  phase(): "idle" | "pan" | "select";
  /**
   * Did the LAST gesture cross the 3px intent threshold? Reset on the next
   * down(). The click handler reads this to skip a click that was really a
   * drag (legacy `dragMoved`, which persisted past mouseup until the next
   * mousedown - reproduced here).
   */
  moved(): boolean;
}

/**
 * Pan a window by a pixel delta, preserving duration and clamping to
 * [minTs,maxTs] with the legacy edge-carry (viewer.html:5286-5300). Exported
 * pure so pointer.test can assert the pan arithmetic directly.
 */
export function panWindowByPixels(
  anchorStart: number,
  anchorEnd: number,
  dxPx: number,
  drawW: number,
  minTs: number,
  maxTs: number,
): { viewStart: number; viewEnd: number } {
  if (drawW <= 0) return { viewStart: anchorStart, viewEnd: anchorEnd };
  const nsPerPx = (anchorEnd - anchorStart) / drawW;
  const shift = -dxPx * nsPerPx;
  let newStart = anchorStart + shift;
  let newEnd = anchorEnd + shift;
  if (newStart < minTs) {
    newEnd += minTs - newStart;
    newStart = minTs;
  }
  if (newEnd > maxTs) {
    newStart -= newEnd - maxTs;
    newEnd = maxTs;
  }
  return {
    viewStart: Math.max(minTs, newStart),
    viewEnd: Math.min(maxTs, newEnd),
  };
}

/** Create a fresh pointer machine (idle). */
export function createPointerMachine(): PointerMachine {
  let state: MachineState = { phase: "idle" };
  // Persists past up() until the next down(), exactly like legacy `dragMoved`.
  let dragMoved = false;

  function snapshot(
    kind: GestureKind,
    startX: number,
    startNs: number,
    curNs: number,
  ): DragSnapshot {
    return { kind, startX, startNs, curNs, moved: dragMoved };
  }

  return {
    down(input: PointerDownInput): PointerCommand[] {
      // A press always resets the moved flag and any stale gesture.
      dragMoved = false;
      if (!input.hasTrace) {
        state = { phase: "idle" };
        return [];
      }
      if (input.shiftKey || input.altKey) {
        // Shift wins over Alt (legacy altZooming = altKey && !shiftKey).
        const kind: "region-select" | "zoom-select" =
          input.altKey && !input.shiftKey ? "zoom-select" : "region-select";
        state = {
          phase: "select",
          kind,
          startX: input.clientX,
          startNs: input.nsAt,
          curNs: input.nsAt,
        };
        return [
          { type: "set-drag", drag: snapshot(kind, input.clientX, input.nsAt, input.nsAt) },
          { type: "cursor", cursor: "crosshair" },
        ];
      }
      // Plain press: pan gesture, anchored to the current window (H6).
      state = {
        phase: "pan",
        startX: input.clientX,
        anchorStart: input.viewStart,
        anchorEnd: input.viewEnd,
        minTs: input.minTs,
        maxTs: input.maxTs,
      };
      return [
        { type: "set-drag", drag: snapshot("pan", input.clientX, input.nsAt, input.nsAt) },
        { type: "cursor", cursor: "grabbing" },
      ];
    },

    move(input: PointerMoveInput): PointerCommand[] {
      if (state.phase === "select") {
        if (Math.abs(input.clientX - state.startX) > DRAG_INTENT_PX) dragMoved = true;
        state.curNs = input.nsAtClamped;
        // Re-write the drag snapshot so the H10 selection overlay redraws the
        // region box from [startNs, curNs] on the transient RAF channel.
        return [
          {
            type: "set-drag",
            drag: snapshot(state.kind, state.startX, state.startNs, state.curNs),
          },
        ];
      }
      if (state.phase === "pan") {
        if (Math.abs(input.clientX - state.startX) > DRAG_INTENT_PX) dragMoved = true;
        const win = panWindowByPixels(
          state.anchorStart,
          state.anchorEnd,
          input.clientX - state.startX,
          input.drawW,
          state.minTs,
          state.maxTs,
        );
        return [{ type: "pan", viewStart: win.viewStart, viewEnd: win.viewEnd }];
      }
      return [];
    },

    up(): PointerCommand[] {
      if (state.phase === "select") {
        const { kind, startNs, curNs } = state;
        const moved = dragMoved;
        state = { phase: "idle" };
        const cmds: PointerCommand[] = [
          { type: "clear-drag" },
          { type: "cursor", cursor: "" },
        ];
        // Only a moved drag commits a region (legacy: `if (dragMoved)`). An
        // unmoved shift/alt press falls through to the click handler, which
        // reads moved() === false and runs the ordinary task select - the
        // legacy behaviour.
        if (moved) {
          cmds.push({
            type: "commit-region",
            kind,
            startNs: Math.min(startNs, curNs),
            endNs: Math.max(startNs, curNs),
          });
        }
        return cmds;
      }
      if (state.phase === "pan") {
        state = { phase: "idle" };
        return [{ type: "clear-drag" }, { type: "cursor", cursor: "" }];
      }
      return [];
    },

    cancel(): PointerCommand[] {
      if (state.phase === "idle") return [];
      state = { phase: "idle" };
      dragMoved = false;
      return [{ type: "clear-drag" }, { type: "cursor", cursor: "" }];
    },

    phase(): "idle" | "pan" | "select" {
      return state.phase;
    },

    moved(): boolean {
      return dragMoved;
    },
  };
}
