// src/pages/flamegraph/fg-keys.ts - the flamegraph page's landing-time
// wiring of the unified keyboard model (T20; docs/ui-inventory/mocks/
// keyboard.html; ticket "flamegraph: `?` help, `/` search unify, `f`
// fit, `z` zoom-undo").
//
// Composition with the frozen widget (features/03 D/N rows) - the router
// NEVER steals a working binding:
//   - `/` and Ctrl/Cmd+F stay the widget's own (its document-level
//     handler runs before our window-level router and preventDefaults,
//     F41); "`/` search unify" means the widget's frame search IS this
//     page's `/` in the unified vocabulary - no palette shadows it.
//   - Escape keeps the widget cascade (F157) via the page's existing
//     window listener; an OPEN unified help overlay closes first (the
//     overlay consumes Escape itself while focused; pages check
//     isOpen() for the focus-elsewhere case - see closeHelpOnEscape).
//   - `?` (dead on the legacy page - K3 verified ?, h, F1 all dead),
//     `f` and `z` (dead - K4) are NEW bindings on previously dead keys.
//
// Zoom undo/fit over the FROZEN widget API: the widget exposes no
// public "set zoom state" - zoomToPath() only ever deepens (it appends
// to the live stack and no-ops on an empty path), and the only public
// paths that shrink the stack are the Escape-cascade reset (which would
// clear the search first - F157 ordering, unacceptable for f/z since
// zoom reset must NOT clear search, F44) and the right-click
// zoom-out-one-level (F94/N rows). So restore composes exactly those
// released behaviors: synthetic contextmenu events pop the stacks to
// root (the widget preventDefaults them, no browser menu appears), then
// zoomToPath() rebuilds the target state. The pops fire onZoomChange;
// a reentrancy guard keeps them out of the history while restoring.
//
// Per architecture 2.5: keys dispatch widget/store actions and announce;
// they never render.

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
   * Fired once after `f`/`z` finish mutating zoom state - exact mode
   * passes the URL sync's onZoomChange (the intermediate pops also fire
   * it, debounced; this final call settles the real state). Api mode
   * omits it: canvas zoom is not URL-synced there (F180).
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

/** The features/03 section-H reference table (F94) - content baseline. */
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
