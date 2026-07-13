// The flamegraph page's wiring of the unified keyboard model: `?` help, `f`
// fit, `z` zoom-undo. `/` and Ctrl/Cmd+F stay the widget's own frame search,
// and Escape keeps the widget's cascade; the new bindings sit on previously
// dead keys.
//
// Zoom undo/fit over the widget's API is indirect: the widget exposes no "set
// zoom state" (zoomToPath only ever deepens), and the only public path that
// shrinks the stack is the right-click zoom-out-one-level. So restore composes
// those released behaviors: synthetic contextmenu events pop the stacks to root
// (the widget preventDefaults them, no browser menu appears), then zoomToPath
// rebuilds the target state. The pops fire onZoomChange; a reentrancy guard
// keeps them out of the history while restoring.

import type { FlamegraphInstance } from "../../lib/canvas/index.js";
import {
  createHelpOverlay,
  createZoomHistory,
  getAnnouncer,
  mountKeyRouter,
} from "../../lib/interact/index.js";
import type { HelpOverlay, HelpSection } from "../../lib/interact/index.js";

/** The widget's zoom state as frame-name paths (getZoomPath's shape). */
type ZoomPaths = { worker: string[]; offworker: string[] };

function pathsEqual(a: ZoomPaths, b: ZoomPaths): boolean {
  return (
    a.worker.join("\t") === b.worker.join("\t") &&
    a.offworker.join("\t") === b.offworker.join("\t")
  );
}

// Depth bound for the pop-to-root loop; a zoom stack deeper than this
// does not occur (stacks grow one entry per user zoom).
const MAX_POPS = 1024;

export interface FgKeysOptions {
  fg: FlamegraphInstance;
  /** The widget's container (canvas lookup for the zoom-out pops). */
  containerEl: HTMLElement;
  /**
   * Fired once after `f`/`z` finish mutating zoom state. Exact mode passes the
   * URL sync's onZoomChange (the intermediate pops also fire it, debounced;
   * this final call settles the real state). Api mode omits it: canvas zoom is
   * not URL-synced there.
   */
  onZoomMutated?: () => void;
}

export interface FgKeys {
  /**
   * Record the current zoom state into the undo history. Pages compose
   * this into the widget's onZoomChange callback; it self-suppresses
   * while `f`/`z` are restoring.
   */
  recordZoom(): void;
  help: HelpOverlay;
  dispose(): void;
}

/** The mouse reference table (help overlay content). */
const MOUSE_ROWS: HelpSection = {
  title: "Mouse",
  rows: [
    { keys: "Click", text: "Zoom into frame" },
    { keys: "Option/Alt + click", text: "Pin tooltip" },
    { keys: "Cmd/Ctrl + click", text: "Open docs.rs" },
    { keys: "Right-click", text: "Zoom out one level" },
  ],
};

const KEY_ROWS: HelpSection = {
  title: "Keyboard",
  rows: [
    { keys: "Cmd/Ctrl + F or /", text: "Search frames" },
    { keys: "f", text: "Fit: reset zoom to the full tree (search kept)" },
    { keys: "z", text: "Undo last zoom (view history)" },
    {
      keys: "Esc",
      text: "Close/clear cascade: tooltip, menus, help, search, zoom",
    },
    { keys: "?", text: "Toggle this help" },
  ],
};

/**
 * Mount the unified keys on a rendered flamegraph page (either mode).
 * Call AFTER the widget exists and any URL zoom restore ran (the current
 * state becomes the undo baseline).
 */
export function mountFlamegraphKeys(options: FgKeysOptions): FgKeys {
  const { fg, containerEl } = options;
  const announcer = getAnnouncer();
  const history = createZoomHistory<ZoomPaths>({ equals: pathsEqual });
  let restoring = false;

  // Baseline: whatever the page landed on (root, or a restored URL zoom).
  history.record(fg.getZoomPath());

  // Pop both zoom stacks to root through the widget's public right-click
  // behavior (see the module header). The worker canvas is the dispatch
  // target; the widget's handler falls back to the off-worker stack once
  // the worker stack is empty.
  function zoomOutAll(): void {
    const canvas = containerEl.querySelector("canvas");
    if (canvas === null) return; // widget torn down; nothing to do
    for (let i = 0; fg.isZoomed() && i < MAX_POPS; i++) {
      canvas.dispatchEvent(
        new MouseEvent("contextmenu", { bubbles: false, cancelable: true }),
      );
    }
  }

  function restore(paths: ZoomPaths): void {
    restoring = true;
    try {
      zoomOutAll();
      if (paths.worker.length > 0) fg.zoomToPath("worker", paths.worker);
      if (paths.offworker.length > 0) fg.zoomToPath("offworker", paths.offworker);
    } finally {
      restoring = false;
    }
    options.onZoomMutated?.();
  }

  function undoZoom(): void {
    const prev = history.undo();
    if (prev === null) {
      announcer.announce("Nothing to undo");
      return;
    }
    restore(prev);
    announcer.announce(`Zoom undo (${history.depth()} more)`);
  }

  function fit(): void {
    if (!fg.isZoomed()) {
      announcer.announce("Already showing the full tree");
      return;
    }
    restoring = true;
    try {
      zoomOutAll();
    } finally {
      restoring = false;
    }
    // One history entry for the whole fit: the pre-fit state stays
    // undoable (`z` right after `f` returns to where you were).
    history.record(fg.getZoomPath());
    options.onZoomMutated?.();
    announcer.announce("Fit: full tree");
  }

  const help = createHelpOverlay(document, {
    title: "Flamegraph - keyboard & mouse",
    sections: [KEY_ROWS, MOUSE_ROWS],
  });

  const disposeRouter = mountKeyRouter(window, [
    { key: "?", onKey: () => help.toggle() },
    { key: "f", onKey: () => fit() },
    { key: "z", onKey: () => undoZoom() },
  ]);

  return {
    recordZoom(): void {
      if (!restoring) history.record(fg.getZoomPath());
    },
    help,
    dispose(): void {
      disposeRouter();
      help.dispose();
    },
  };
}

/**
 * Escape composition for the page's existing window listener: an open
 * unified help overlay closes INSTEAD of running the widget cascade
 * (needed when focus is outside the overlay; the overlay consumes its
 * own Escape while focused). Returns true when it consumed the press.
 */
export function closeHelpOnEscape(keys: FgKeys): boolean {
  if (keys.help.isOpen()) {
    keys.help.close();
    return true;
  }
  return false;
}
