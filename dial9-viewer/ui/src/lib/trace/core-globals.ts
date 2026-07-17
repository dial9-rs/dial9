// Seeds the frozen core's browser globals that its own modules look up at
// runtime, for bundled ESM pages where `typeof require` is undefined:
//
//   * `TraceParser` - trace_analysis.js's factory reads it AT FACTORY TIME,
//     so it must exist BEFORE the trace_analysis.js module initializer runs;
//     every src module that imports trace_analysis.js therefore lists this
//     module as its FIRST import (ESM evaluation is depth-first in
//     declaration order).
//   * `TraceDecoder` - trace_parser.js's getTraceDecoder() reads it at PARSE
//     TIME (not module-init), falling back to this global when `require` is
//     absent, else throwing "TraceDecoder not found". The worker entry
//     (worker/trace-worker.ts) seeds it for the off-thread parse; seeding it
//     here covers the MAIN-THREAD parse (loadTraceOnMainThread), which runs
//     the same frozen parser in the page's own global scope.
//
// Node/vitest never needs either (the real CJS require chain works there);
// running this is harmless.

import * as traceParser from "../../../trace_parser.js";
import { TraceDecoder } from "../../../decode.js";

// CJS interop exposes module.exports as `default`; fall back to the
// namespace for transforms that only provide named bindings.
(globalThis as Record<string, unknown>)["TraceParser"] =
  (traceParser as { default?: unknown }).default ?? traceParser;
(globalThis as Record<string, unknown>)["TraceDecoder"] = TraceDecoder;
