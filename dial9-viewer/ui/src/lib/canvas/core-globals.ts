// Seeds the `TraceAnalysis` browser global for bundled entries.
// flamegraph.js and flamegraph_export.js read it at factory time, so it
// must be seeded before either initializes; trace_analysis.js needs
// `TraceParser` first, hence the chained seed import (declaration order is
// evaluation order).

import "../trace/core-globals.js";
import * as traceAnalysis from "../../../trace_analysis.js";

(globalThis as Record<string, unknown>)["TraceAnalysis"] =
  (traceAnalysis as { default?: unknown }).default ?? traceAnalysis;
