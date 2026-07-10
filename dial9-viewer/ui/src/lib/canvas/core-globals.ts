// lib/canvas/core-globals.ts - seeds the `TraceAnalysis` browser global
// for bundled entries (T13; see lib/trace/core-globals.ts for the full
// rationale). flamegraph.js and flamegraph_export.js both read the
// `TraceAnalysis` global at factory time in a bundle, so it must be
// seeded before either module initializes; trace_analysis.js itself needs
// `TraceParser` first, hence the chained seed import (declaration order
// is evaluation order).

import "../trace/core-globals.js";
import * as traceAnalysis from "../../../trace_analysis.js";

(globalThis as Record<string, unknown>)["TraceAnalysis"] =
  (traceAnalysis as { default?: unknown }).default ?? traceAnalysis;
