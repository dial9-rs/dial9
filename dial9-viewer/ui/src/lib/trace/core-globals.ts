// lib/trace/core-globals.ts - seeds the `TraceParser` browser global for
// bundled entries (T13; the generalization of the T16 worker-entry
// TraceDecoder lesson, see vite.config.ts commonjsOptions).
//
// The frozen core resolves its intra-core dependencies via `typeof
// require` (Node) or browser globals that the legacy pages establish with
// <script src> ordering: trace_analysis.js's factory reads the
// `TraceParser` global AT FACTORY TIME. In a bundled ESM page `typeof
// require` is genuinely "undefined" (commonjsOptions.ignoreDynamicRequires
// keeps it that way by design), so the global must exist BEFORE the
// trace_analysis.js module initializer runs. ESM evaluation is depth-first
// in declaration order, so every src module that imports
// trace_analysis.js directly lists this module as its FIRST import.
//
// Node/vitest never needs this (the real CJS require chain works there);
// running it is harmless.

import * as traceParser from "../../../trace_parser.js";

// The CJS interop exposes module.exports as `default`; fall back to the
// namespace for transforms that only provide named bindings. Either shape
// carries the full trace_parser.js api the core consumers read.
(globalThis as Record<string, unknown>)["TraceParser"] =
  (traceParser as { default?: unknown }).default ?? traceParser;
