// lib/interact/kb-selection.ts - the keyboard region/zoom selection machine
// (T23; features/02 H9, docs/ui-inventory/02-architecture.md 2.5).
//
// Pressing Shift (region) or Alt (zoom) with no drag starts a keyboard-driven
// selection: a cursor seeded at the mouse (if in view) or the view centre,
// extended by Left/Right arrows (5% of the visible duration per step), then
// confirmed (Shift/Alt again, or Enter) or cancelled (Escape). Ported from
// viewer.html:6228-6268 + completeKbSelection/cancelKbSelection (:5216-5268).
//
// A PURE, DOM-free machine (same shape as pointer.ts): it returns COMMANDS the
// binding executes as store actions. The binding supplies the seed timestamp,
// the per-step ns, and the clamp bounds (all layout/viewport-derived), so the
// machine is unit-testable over plain numbers. It never renders (F2).
//
// Region vs zoom on confirm: a region selection emits `commit-region` with
// kind "region-select" (the binding writes selection.sidebarRange; T32 opens
// the flamegraph/blocking/heap panel by data present - the H7 seam). A zoom
// selection emits kind "zoom-select" (the binding zooms the viewport, H8).
// Both announce through the A16 live region (T21) - feedback only.

import type { KeyboardSelection } from "../../types/state.js";

/**
 * Minimum selection span to commit (legacy completeKbSelection `< 100`,
 * viewer.html:5238). A smaller keyboard selection is silently discarded.
 */
export const MIN_SELECTION_NS = 100;

/** The selection mode, matching KeyboardSelection.kind / DragKind. */
export type SelectionMode = "region-select" | "zoom-select";

/** Left (-1, earlier) / Right (+1, later) cursor extension. */
export type ExtendDirection = -1 | 1;

/**
 * Commands the machine emits; the binding maps each to a store action.
 *   - set-kb:        write `transient.keyboardSelection` (start / extend / clear);
 *   - commit-region: a completed selection [startNs,endNs] to open/zoom;
 *   - announce:      an ARIA/toast status string (A16; feedback only).
 */
export type KbSelectionCommand =
  | { type: "set-kb"; selection: KeyboardSelection | null }
  | { type: "commit-region"; kind: SelectionMode; startNs: number; endNs: number }
  | { type: "announce"; message: string };

const START_MESSAGE: Record<SelectionMode, string> = {
  // ASCII-only wording (no em-dash / arrow glyphs); semantics preserved.
  "region-select":
    "Region selection started. Use Left and Right arrows to extend, Shift or Enter to confirm.",
  "zoom-select":
    "Zoom selection started. Use Left and Right arrows to extend, Alt or Enter to confirm.",
};

const CONFIRM_MESSAGE: Record<SelectionMode, string> = {
  "region-select": "Selected region opened",
  "zoom-select": "Zoomed to selected region",
};

export interface KbSelectionMachine {
  /** True while a keyboard selection is in progress. */
  active(): boolean;
  /** The active mode, or null when idle. */
  mode(): SelectionMode | null;
  /**
   * Start a selection at `seedNs`. If a selection is already active, this is a
   * toggle: SAME mode confirms it, a DIFFERENT mode is ignored (legacy: the
   * Shift/Alt branch only completes when kbSelMode === mode). Returns the
   * commands to run.
   */
  start(mode: SelectionMode, seedNs: number): KbSelectionCommand[];
  /** Move the cursor one step (`stepNs`) in `dir`, clamped to [minTs,maxTs]. */
  extend(dir: ExtendDirection, stepNs: number, minTs: number, maxTs: number): KbSelectionCommand[];
  /** Confirm the selection (Enter): commit when >= 100ns, else silently drop. */
  confirm(): KbSelectionCommand[];
  /** Cancel the selection (Escape): clear + announce. */
  cancel(): KbSelectionCommand[];
  /** Clear WITHOUT announcing (e.g. a mousedown pre-empts the selection). */
  clear(): KbSelectionCommand[];
}

/** Create a fresh keyboard-selection machine (idle). */
export function createKbSelectionMachine(): KbSelectionMachine {
  let sel: KeyboardSelection | null = null;

  function snapshot(): KeyboardSelection {
    // Return a fresh object each time (the store replaces slices wholesale and
    // dev-freezes them; never hand it a reference we later mutate).
    const s = sel as KeyboardSelection;
    return { kind: s.kind, startNs: s.startNs, cursorNs: s.cursorNs };
  }

  return {
    active(): boolean {
      return sel !== null;
    },
    mode(): SelectionMode | null {
      return sel === null ? null : sel.kind;
    },

    start(mode: SelectionMode, seedNs: number): KbSelectionCommand[] {
      if (sel !== null) {
        // Toggle: same mode confirms; different mode is a no-op (legacy).
        if (sel.kind === mode) return this.confirm();
        return [];
      }
      sel = { kind: mode, startNs: seedNs, cursorNs: seedNs };
      return [
        { type: "set-kb", selection: snapshot() },
        { type: "announce", message: START_MESSAGE[mode] },
      ];
    },

    extend(dir: ExtendDirection, stepNs: number, minTs: number, maxTs: number): KbSelectionCommand[] {
      if (sel === null) return [];
      const next = Math.max(minTs, Math.min(maxTs, sel.cursorNs + dir * stepNs));
      sel = { kind: sel.kind, startNs: sel.startNs, cursorNs: next };
      return [{ type: "set-kb", selection: snapshot() }];
    },

    confirm(): KbSelectionCommand[] {
      if (sel === null) return [];
      const kind = sel.kind;
      const start = Math.min(sel.startNs, sel.cursorNs);
      const end = Math.max(sel.startNs, sel.cursorNs);
      sel = null;
      // Too small: clear the cursor, commit nothing (legacy `< 100` -> hide).
      if (end - start < MIN_SELECTION_NS) {
        return [{ type: "set-kb", selection: null }];
      }
      return [
        { type: "set-kb", selection: null },
        { type: "commit-region", kind, startNs: start, endNs: end },
        { type: "announce", message: CONFIRM_MESSAGE[kind] },
      ];
    },

    cancel(): KbSelectionCommand[] {
      if (sel === null) return [];
      sel = null;
      return [
        { type: "set-kb", selection: null },
        { type: "announce", message: "Selection cancelled" },
      ];
    },

    clear(): KbSelectionCommand[] {
      if (sel === null) return [];
      sel = null;
      return [{ type: "set-kb", selection: null }];
    },
  };
}
