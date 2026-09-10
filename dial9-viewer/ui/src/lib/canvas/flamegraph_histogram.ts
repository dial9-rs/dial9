// Typed seam over the frozen poll-duration histogram helpers
// (flamegraph_histogram.js): the DOM-free math behind the aggregated-mode
// minimap - histogram normalization, the sample-weighted median (fast/slow
// split seed), bar layout, and pixel <-> poll-duration-band mapping. The
// api-mode page composes the minimap through this re-export instead of
// importing the core module directly. Duration labels come from
// `formatHumanDuration` (lib/trace), the viewer's one duration format.
//
// flamegraph_histogram.js is self-contained (no dependencies), so it needs no
// global-seed chain like the flamegraph widget does.

export {
  normalizeHistogram,
  sampleWeightedMedianNs,
  histogramLayout,
  pxToNs,
  brushToBand,
} from "../../../flamegraph_histogram.js";
export type {
  PollHistogramBar,
  HistogramColumn,
  HistogramLayout,
  PollBand,
} from "../../../flamegraph_histogram.js";
