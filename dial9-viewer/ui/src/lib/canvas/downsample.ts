// Typed re-exports of the frozen core's pixel-bounding fill primitives.
// The implementations live in trace_analysis.js (typed by
// src/types/trace_analysis.d.ts); this module is the lib/canvas seam so
// canvas components import their drawing math from one place.
//
// Contracts enforced by the core:
// - pixelDownsampleSpans: at most ONE representative span per pixel column
//   (largest weight whose start falls in the column); `spans` MUST be
//   sorted ascending by start.
// - makeBarCoalescer: collapses adjacent same-color pixel bars into one
//   fillRect per run; bars MUST be pushed in ascending x1 order and
//   flush() called after the last push.
// - pixelCoverage: per-column 0..1 coverage histogram for a sorted,
//   non-overlapping span array.

// MUST precede the trace_analysis.js import: its factory reads the
// TraceParser browser global in bundled entries (see core-globals.ts).
import "./core-globals.js";

export {
  makeBarCoalescer,
  pixelCoverage,
  pixelDownsampleSpans,
} from "../../../trace_analysis.js";
