// Typed re-exports of the frozen core's density-timeline data helpers.
// The implementations live in heatmap.js (typed by src/types/heatmap.d.ts);
// this module is the lib/canvas seam so the page imports its density math
// from one place. Do NOT re-implement any of this math: the frozen helpers
// are the source of truth for seam tiling, gap detection, boot transitions
// and density accumulation.

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
