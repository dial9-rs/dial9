// lib/canvas/export-globals.ts - seeds the `FlamegraphExport` browser
// global for bundled entries (T13; see lib/trace/core-globals.ts for the
// full rationale). flamegraph.js resolves the export module at factory
// time via this global in a bundle; without it the widget silently takes
// its F66 "export module missing" degradation and hides the Export menu.
// flamegraph_export.js's own factory needs `TraceAnalysis`, hence the
// chained seed import first.

import "./core-globals.js";
import * as flamegraphExport from "../../../flamegraph_export.js";

(globalThis as Record<string, unknown>)["FlamegraphExport"] =
  (flamegraphExport as { default?: unknown }).default ?? flamegraphExport;
