// The tokio-stats page's wiring of the unified keyboard model: the `?` help
// overlay plus Escape to close it (the page has no other keyboard bindings).

import {
  createHelpOverlay,
  isTextEntryTarget,
  mountKeyRouter,
} from "../../lib/interact/index.js";
import type { HelpOverlay, HelpSection } from "../../lib/interact/index.js";

/** The keyboard reference shown in the help overlay. */
const KEY_ROWS: HelpSection = {
  title: "Keyboard",
  rows: [
    { keys: "?", text: "Toggle this help" },
    { keys: "Esc", text: "Close this help" },
  ],
};

const MOUSE_ROWS: HelpSection = {
  title: "Controls",
  rows: [
    { keys: "Load", text: "Fetch all periods (server-side refinement)" },
    { keys: "Threshold", text: "Filter to polls above the slider duration" },
    { keys: "+ Add period", text: "Add a comparison window (diff view)" },
    { keys: "Off/On-CPU count", text: "Open the worst poll's window in the viewer" },
  ],
};

export interface TokioStatsKeys {
  help: HelpOverlay;
  dispose(): void;
}

/** Mount the `?` help overlay + its Escape/close wiring on the page. */
export function mountTokioStatsKeys(): TokioStatsKeys {
  const help = createHelpOverlay(document, {
    title: "Tokio Stats - keyboard & controls",
    sections: [KEY_ROWS, MOUSE_ROWS],
  });

  const disposeRouter = mountKeyRouter(window, [
    { key: "?", onKey: () => help.toggle() },
  ]);

  // Focus-outside Escape: the overlay consumes its own Escape while focused;
  // this covers the case where focus is elsewhere. The page has no other Escape
  // behavior to cascade to.
  function onKeydown(e: KeyboardEvent): void {
    if (e.key !== "Escape") return;
    if (isTextEntryTarget(e.target)) return;
    if (help.isOpen()) help.close();
  }
  window.addEventListener("keydown", onKeydown);

  return {
    help,
    dispose(): void {
      disposeRouter();
      window.removeEventListener("keydown", onKeydown);
      help.dispose();
    },
  };
}
