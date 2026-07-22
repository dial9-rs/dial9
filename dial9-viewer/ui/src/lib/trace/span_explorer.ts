// Typed seam over the frozen span_explorer.js: catalog sorting/labelling,
// log-duration histogram geometry, percentile estimation, the five-way time
// composition, attribute filters, and the flamegraph/viewer deep-link builders.
// Shared verbatim with the raw-trace path so the catalog a client builds from
// trace bytes and the one the server aggregates cannot drift.
//
// lib/trace is the sanctioned frozen-core import boundary; the Span Explorer
// page consumes all of this through the barrel.

export {
  TIME_CATEGORIES,
  addAttrFilter,
  bandComposition,
  buildLogHistogram,
  buildSpanCatalog,
  classifyExemplarSnapshot,
  collectExemplarAttributeKeys,
  columnIsDegenerate,
  completeExemplarRefresh,
  computeTimeComposition,
  countInBand,
  decodeSpanExplorerState,
  durationAtPercentile,
  encodeSpanExplorerState,
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
  parseSpanEventName,
  percentileForDuration,
  removeAttrFilter,
  sameSpanCatalogStatistics,
  setMaxFilesParam,
  shouldAdoptCatalogSnapshot,
  sortSpanTypes,
  spanBrushToBand,
  spanHistogramLayout,
  spanNsToPx,
  spanPxToNs,
  spanTypeLabel,
  spanTypeQuality,
} from "../../../span_explorer.js";

export type {
  AttrFilter,
  BandCompositionSums,
  CompositionCategory,
  DecodedSpanExplorerState,
  DurationBand,
  ExemplarLinkScope,
  HistogramBarLike,
  ParsedSpanEventName,
  SpanExplorerState,
  SpanHistogramBar,
  SpanHistogramColumn,
  SpanHistogramLayout,
  StreamMode,
  TimeCompositionView,
} from "../../../span_explorer.js";
