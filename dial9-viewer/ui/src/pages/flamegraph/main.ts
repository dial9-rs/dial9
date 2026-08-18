// The flamegraph page entry: a typed shell around the flamegraph widget that
// serves three modes from one entry -
//   - diff mode (`?diff=1&a=..&b=..`): two-sided A/B differential (diff-mode.ts);
//   - aggregated mode (`?api=1`): the server's demand-driven /api/flamegraph
//     refinement loop (api-mode.ts);
//   - exact mode (default): client fetch + decode of `?trace=` components
//     (exact-mode.ts).

import { parseDiff } from "../../lib/canvas/index.js";
import { pageEls } from "./dom.js";
import { runApiMode } from "./api-mode.js";
import { runDiffMode } from "./diff-mode.js";
import { runExactMode } from "./exact-mode.js";

const params = new URLSearchParams(window.location.search);
const els = pageEls();

if (parseDiff(params) !== null) {
  runDiffMode(params, els);
} else if (params.get("api") === "1") {
  runApiMode(params, els);
} else {
  // Errors surface through els.showError inside; an unexpected throw is an
  // unhandled rejection on the console.
  void runExactMode(params, els);
}
