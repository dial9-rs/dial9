// Typed seam over the shared span_explorer.js: catalog sorting/labelling,
// log-duration histogram geometry, percentile estimation, the five-way time
// composition, attribute filters, and the flamegraph deep-link builder.
//
// lib/trace is the sanctioned shared-core import boundary; the Span Explorer
// page consumes all of this through the barrel.

import { formatHumanDuration } from "../../../format.js";

/**
 * Duration for a Span Explorer table cell. The missing/invalid and zero
 * contracts are this page's ("—" reads as no measurement, a bare "0" as a
 * measured nothing); the number itself goes through the viewer's one duration
 * format.
 */
export function fmtNs(ns: number | string | null | undefined): string {
  if (ns == null) return "—";
  const n = Number(ns);
  if (!Number.isFinite(n) || n < 0) return "—";
  if (n === 0) return "0";
  return formatHumanDuration(n);
}

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
  exemplarsInBand,
  flamegraphUrl,
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
  HistogramBarLike,
  SpanExplorerState,
  SpanHistogramBar,
  SpanHistogramColumn,
  SpanHistogramLayout,
  StreamMode,
  TimeCompositionView,
} from "../../../span_explorer.js";
