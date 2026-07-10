// lib/canvas/downsample.ts - typed re-exports of the frozen core's
// pixel-bounding fill primitives (T08; architecture 2.3 N1: "the tricks
// that make the viewer fast are named, tested contracts").
//
// The implementations live in trace_analysis.js (frozen, untouchable -
// ADR-0004 section 6) and are typed by src/types/trace_analysis.d.ts;
// this module is the lib/canvas seam so canvas components import their
// drawing math from one place instead of reaching into the core.
//
// Contracts (enforced by the core, documented here):
// - pixelDownsampleSpans: at most ONE representative span per pixel
//   column (largest weight whose start falls in the column); `spans`
//   MUST be sorted ascending by start.
// - makeBarCoalescer: collapses adjacent same-color pixel bars into one
//   fillRect per run; bars MUST be pushed in ascending x1 order and
//   flush() called after the last push.
// - pixelCoverage: per-column 0..1 coverage histogram for a sorted,
//   non-overlapping span array.
//
// The stroke-side counterpart (per-column vertices for LINES, not fills)
// is ./stroke.ts - the core has no stroke downsampler (that gap is F1).

import { makeBarCoalescer } from "../../../trace_analysis.js";

export {
  makeBarCoalescer,
  pixelCoverage,
  pixelDownsampleSpans,
} from "../../../trace_analysis.js";

/** The coalescer handle makeBarCoalescer returns (push per bar + flush). */
export type BarCoalescer = ReturnType<typeof makeBarCoalescer>;
