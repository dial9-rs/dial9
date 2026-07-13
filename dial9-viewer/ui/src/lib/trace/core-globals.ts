// Seeds the `TraceParser` browser global that trace_analysis.js's factory
// reads AT FACTORY TIME. In a bundled ESM page `typeof require` is
// undefined, so the global must exist BEFORE the trace_analysis.js module
// initializer runs; every src module that imports trace_analysis.js
// therefore lists this module as its FIRST import (ESM evaluation is
// depth-first in declaration order). Node/vitest never needs it (the real
// CJS require chain works there); running it is harmless.

import * as traceParser from "../../../trace_parser.js";

// CJS interop exposes module.exports as `default`; fall back to the
// namespace for transforms that only provide named bindings.
(globalThis as Record<string, unknown>)["TraceParser"] =
  (traceParser as { default?: unknown }).default ?? traceParser;
