// Typed seam over the shared span_explorer.js: catalog sorting/labelling,
// log-duration histogram geometry, percentile estimation, the five-way time
// composition, attribute filters, and the flamegraph/viewer deep-link builders.
//
// lib/trace is the sanctioned shared-core import boundary; the Span Explorer
// page consumes all of this through the barrel.

export {
  TIME_CATEGORIES,
  addAttrFilter,
  classifyExemplarSnapshot,
  collectExemplarAttributeKeys,
  columnIsDegenerate,
  completeExemplarRefresh,
  computeTimeComposition,
  countInBand,
  durationAtPercentile,
  exemplarAttrValue,
  exemplarRequestMatches,
  exemplarViewerUrl,
  exemplarsInBand,
  flamegraphUrl,
  fmtNs,
  fmtPercentile,
  formatAttrFilterParams,
  hasAttrFilter,
  mergeSelectedExemplarSnapshot,
  normalizeSpanHistogram,
  parseAttrFilterParams,
  percentileForDuration,
  removeAttrFilter,
  setMaxFilesParam,
  shouldAdoptCatalogSnapshot,
  sortSpanTypes,
  spanBrushToBand,
  spanHistogramLayout,
  spanNsToPx,
  spanTypeLabel,
  spanTypeQuality,
} from "../../../span_explorer.js";

export type {
  AttrFilter,
  CompositionCategory,
  DurationBand,
  ExemplarLinkScope,
  HistogramBarLike,
  SpanExplorerState,
  SpanHistogramBar,
  SpanHistogramColumn,
  SpanHistogramLayout,
  StreamMode,
  TimeCompositionView,
} from "../../../span_explorer.js";
