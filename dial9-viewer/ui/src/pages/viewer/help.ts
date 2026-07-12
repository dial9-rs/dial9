// src/pages/viewer/help.ts - the viewer's `?` help overlay (T21;
// features/02 section T). Uses the unified help-overlay COMPONENT (T20,
// lib/interact) with viewer-specific content, and hooks it into the
// shell's Escape cascade at the highest priority (the legacy D9 order:
// help closes first).
//
// Content is the viewer's shortcut vocabulary (features/02 T4 mouse +
// keyboard reference), extended with the unified-keyboard additions the
// shell's status bar and hint chips advertise (K3: `/` search, `n`/`p`
// POI, `f` fit, `z` undo zoom, `g` goto time). The pointer/zoom/selection
// gestures themselves are wired by T22/T23; the help overlay is the static
// reference for them, which is exactly what T4 records.

import { createHelpOverlay } from "../../lib/interact/index.js";
import type { HelpOverlay, HelpSection } from "../../lib/interact/index.js";
import { ESC_PRIORITY } from "./esc-cascade.js";
import type { EscCascade } from "./esc-cascade.js";

const VIEWER_HELP_SECTIONS: readonly HelpSection[] = [
  {
    title: "Keyboard",
    rows: [
      { keys: "/", text: "Search tasks, spans, and points of interest" },
      { keys: "n / p", text: "Step to the next / previous point of interest" },
      { keys: "f", text: "Fit the whole trace in view" },
      { keys: "z", text: "Undo the last zoom" },
      { keys: "g", text: "Go to a specific time" },
      { keys: "W / S", text: "Zoom in / out on the timeline" },
      { keys: "A / D", text: "Pan the timeline left / right" },
      { keys: "Up / Down", text: "Zoom in / out" },
      { keys: "Left / Right", text: "Pan the view" },
      { keys: "Shift", text: "Start a region selection (arrows extend, Enter confirms)" },
      { keys: "Option / Alt", text: "Start a zoom selection" },
      { keys: "Esc", text: "Close the topmost surface, then clear selection" },
      { keys: "?", text: "Toggle this help" },
    ],
  },
  {
    title: "Mouse",
    rows: [
      { keys: "Scroll", text: "Scroll the worker lanes" },
      { keys: "Cmd/Ctrl + Scroll", text: "Zoom centered on the cursor" },
      { keys: "Drag", text: "Pan the view" },
      { keys: "Click a poll", text: "Select its task and open the inspector" },
      { keys: "Shift + Drag", text: "Select a region to analyze" },
      { keys: "Option/Alt + Drag", text: "Zoom into a region" },
    ],
  },
];

export interface ViewerHelp {
  overlay: HelpOverlay;
  /** Toggle the overlay (bind to `?`). */
  toggle(): void;
  /** Detach the overlay and its esc-cascade registration. */
  dispose(): void;
}

/**
 * Mount the viewer help overlay into `doc` and register it in the shell's
 * Escape cascade (highest priority). Returns the handle the shell binds
 * `?` to.
 */
export function mountViewerHelp(doc: Document, esc: EscCascade): ViewerHelp {
  const overlay = createHelpOverlay(doc, {
    title: "Trace viewer - shortcuts",
    sections: VIEWER_HELP_SECTIONS,
  });
  const unregister = esc.register({
    name: "help",
    priority: ESC_PRIORITY.help,
    isOpen: () => overlay.isOpen(),
    close: () => overlay.close(),
  });
  return {
    overlay,
    toggle: () => overlay.toggle(),
    dispose(): void {
      unregister();
      overlay.dispose();
    },
  };
}
