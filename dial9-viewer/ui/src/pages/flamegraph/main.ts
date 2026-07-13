// The flamegraph page entry: a typed shell around the flamegraph widget that
// serves both modes from one entry -
//   - exact mode (default): client fetch + decode of `?trace=` components
//     (exact-mode.ts);
//   - aggregated mode (`?api=1`): the server's demand-driven /api/flamegraph
//     refinement loop (api-mode.ts).
//
// The shell loads /ui-switch.js via a plain <script> tag; mounting with side
// "new" renders the "Switch to legacy UI" pill.

import { pageEls } from "./dom.js";
import { runApiMode } from "./api-mode.js";
import { runExactMode } from "./exact-mode.js";

if (window.D9UiSwitch) {
  window.D9UiSwitch.mount({ side: "new" });
} else {
  // One-time load-order/serving problem, not a loop - log it loudly.
  console.warn("ui-switch.js is not loaded; the UI switch control is unavailable");
}

const params = new URLSearchParams(window.location.search);
const els = pageEls();

if (params.get("api") === "1") {
  runApiMode(params, els);
} else {
  // Errors surface through els.showError inside; an unexpected throw is an
  // unhandled rejection on the console.
  void runExactMode(params, els);
}
