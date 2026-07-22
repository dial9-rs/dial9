// Seeds the `FlamegraphExport` browser global for bundled entries.
// flamegraph.js resolves the export module at factory time via this
// global; without it the widget silently hides the Export menu.
// flamegraph_export.js's own factory needs `TraceAnalysis`, hence the
// chained seed import first.

import "./core-globals.js";
import * as flamegraphExport from "../../../flamegraph_export.js";

(globalThis as Record<string, unknown>)["FlamegraphExport"] =
  (flamegraphExport as { default?: unknown }).default ?? flamegraphExport;
