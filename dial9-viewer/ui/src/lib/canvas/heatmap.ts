// lib/canvas/heatmap.ts - typed re-exports of the frozen core's S3-browser
// density-timeline data helpers (T14; architecture 2.3/2.7, features/01 F).
//
// The implementations live in heatmap.js (frozen, untouchable - AGENTS.md
// core list) and are typed by src/types/heatmap.d.ts; this module is the
// lib/canvas seam so the browser page imports its density math from one
// place instead of reaching into the core (same pattern as ./downsample.ts
// over trace_analysis.js). Do NOT re-implement any of this math: the
// frozen helpers are the source of truth for seam tiling, gap detection,
// boot transitions and the density accumulation the heatmap paints.

export {
  MAX_OPEN_BYTES,
  MIN_SEGMENT_SECONDS,
  accumulateDensity,
  bootTransitions,
  densityColor,
  groupByHost,
  segmentGaps,
  segmentSpan,
  segmentsOverlapping,
  tileSegments,
  totalBytes,
} from "../../../heatmap.js";
export type { HostRow, SegmentInput } from "../../../heatmap.js";
