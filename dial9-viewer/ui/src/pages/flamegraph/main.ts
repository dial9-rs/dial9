// src/pages/flamegraph/main.ts - the migrated flamegraph page entry (T13;
// Vite entry new/flamegraph.html; ADR-0004 section 8 stage 2). The first
// page on the new stack: a typed shell around the frozen flamegraph
// widget, behavior-identical to the legacy /flamegraph.html (features/03
// is the contract).
//
// Both legacy modes are served from this one entry, exactly like the
// legacy page's inline bootstrap:
//   - exact mode (default): client fetch + decode of `?trace=` components
//     (exact-mode.ts, via the T16 worker load pipeline);
//   - aggregated mode (`?api=1`): the server's demand-driven
//     /api/flamegraph refinement loop (api-mode.ts, features/03 section P).
//
// The dual-UI switch (T38): the shell loads /ui-switch.js via a plain
// <script> tag (the same copied file the legacy pages load); mounting
// with side "new" renders the always-visible "Switch to legacy UI" pill.
// The `?ui=` dispatch convention applies to the canonical URL only, so
// this side never redirects.

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
  // Errors surface through els.showError inside; an unexpected throw is
  // an unhandled rejection on the console, matching the legacy IIFE.
  void runExactMode(params, els);
}
