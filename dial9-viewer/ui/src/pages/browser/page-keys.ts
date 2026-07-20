// The browser page's wiring of the unified keyboard model.
//
// Bindings:
//   - `?` toggles the unified help overlay;
//   - `/` focuses the active tab's search input (browse: the prefix
//     field, the text input the Search button's readiness gates on;
//     raw: the raw key-substring input) and selects its text;
//   - Enter in the browse controls-bar fields (bucket, prefix, From/To
//     pickers) clicks Search when it is enabled. The raw tab keeps its own
//     Enter handler (raw-view.ts).
//
// Keys dispatch actions/focus only, never render.

import {
  createHelpOverlay,
  helpKeyBindings,
  mountKeyRouter,
} from "../../lib/interact/index.js";
import type { HelpSection } from "../../lib/interact/index.js";
import type { PageCtx } from "./ctx.js";

const KEY_ROWS: HelpSection = {
  title: "Keyboard",
  rows: [
    { keys: "/", text: "Focus the search field (browse: prefix, raw: key substring)" },
    { keys: "Enter", text: "Run the search (in any controls-bar field)" },
    {
      keys: "Shift",
      text: "Heatmap focused: start a window selection, Shift again confirms",
    },
    { keys: "Left / Right", text: "Extend the heatmap window selection" },
    { keys: "Enter", text: "Confirm the heatmap window selection" },
    { keys: "Esc", text: "Cancel the selection / close this help" },
    { keys: "?", text: "Toggle this help" },
  ],
};

// The heatmap's existing mouse paths, so the overlay documents the full
// selection story, not just the new keys.
const MOUSE_ROWS: HelpSection = {
  title: "Mouse (heatmap)",
  rows: [
    { keys: "Drag", text: "Select a window of segments" },
    { keys: "Alt + drag", text: "Zoom the time axis" },
    { keys: "Click", text: "Select a single segment" },
    { keys: "Double-click", text: "Reset zoom" },
  ],
};

export function mountBrowserPageKeys({ store, els, actions }: PageCtx): void {
  const help = createHelpOverlay(document, {
    title: "Trace browser - keyboard",
    sections: [KEY_ROWS, MOUSE_ROWS],
  });

  // Enter submits the browse (time-range) search from any controls-bar
  // field, exactly like clicking Search - including its disabled gate (the
  // readiness formula renders into the button).
  const submitOnEnter = (e: KeyboardEvent): void => {
    if (e.key !== "Enter") return;
    if (els.searchBtn.disabled) return;
    e.preventDefault();
    void actions.doTimeRangeSearch();
  };
  for (const el of [els.bucketInput, els.prefixInput, els.rangeFrom, els.rangeTo]) {
    el.addEventListener("keydown", submitOnEnter);
  }

  mountKeyRouter(window, [
    ...helpKeyBindings(help),
    {
      key: "/",
      onKey: (): void => {
        const target =
          store.getState().ui.tab === "raw" ? els.rawSearchInput : els.prefixInput;
        target.focus();
        target.select();
      },
    },
  ]);
}
