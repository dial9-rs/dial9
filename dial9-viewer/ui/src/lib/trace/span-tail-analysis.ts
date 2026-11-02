/** Wire types returned by the Rust /api/span-tail analysis endpoint. */
export interface TailRecord {
  elapsedMs: number;
  insideMs: number | null;
  startNs: string;
  endNs: string;
  sourceUrl: string;
  requestId: string | null;
  /** Original recorded string pairs, preserving duplicate keys and their order.
   * null/absent means unavailable; [] means no pairs are included.
   * [] with attributesOmitted=0 means this span recorded no fields. */
  attributes?: [string, string][] | null;
  /** Whole pairs omitted by response limits; null/absent means unavailable. */
  attributesOmitted?: number | null;
}
export interface NumericField {
  key: string;
  unit: string | null;
}
export interface TailSelection {
  percentile: number;
  groupKey: string | null;
  groupValue: string | null;
  minMs: number | null;
  maxMs: number | null;
}
export interface TailValueAxis {
  /** Declared compatible time units normalize to ms; unknown units remain null. */
  unit: string | null;
  /** Multiply raw point/summary values by this number for the display axis. */
  multiplier: number;
  minimum: number;
  maximum: number;
}
export interface TailDensity {
  elapsedEdgesMs: number[];
  /** Edges already use the display unit in valueAxis. */
  valueEdges: number[];
  /** Row-major: counts[y * (elapsedEdgesMs.length - 1) + x]. */
  counts: number[];
  /** Every loaded observation with this field; points are a navigation sample. */
  count: number;
  maxCount: number;
}
export interface TailDifference {
  /** Exact maximum CDF separation between all recorded cohort values. */
  cdfGap: number;
  /** Descriptive sampling allowance, assuming independent observations. */
  samplingMargin: number;
  /** max(0, cdfGap - samplingMargin); this is not a causal or p-value claim. */
  lowerBound: number;
  /** Number of eligible numeric fields covered by the sampling budget. */
  comparisons: number;
  samplingBudget: number;
  /** All four summaries use original producer units and all recorded values. */
  typicalMedian: number;
  slowMedian: number;
  typicalP99: number;
  slowP99: number;
}
export interface TailSignal {
  key: string;
  unit: string | null;
  typical: number | null;
  slow: number | null;
  typicalCount: number;
  slowCount: number;
  typicalCoverage: number;
  slowCoverage: number;
  separation: number | null;
  correlation: number | null;
  correlationCount: number;
  duplicatesLatency: boolean;
  filesCompared: number;
  filesAgree: number;
  points: [number, number, TailRecord][];
  /** Optional only for compatibility with previously cached reports. */
  valueAxis?: TailValueAxis;
  density?: TailDensity;
  /** null means one of the cohorts has no recorded values; absent is an old report. */
  tailDifference?: TailDifference | null;
}
export interface TailComposition {
  count: number;
  detailedCount: number;
  detailedCoverage: number;
  categories: { key: string; share: number; meanMs: number }[];
}
export interface CategoryLatency {
  value: string;
  count: number;
  shares: number[];
  /** Exact per-bin counts, optional for previously cached reports. */
  binCounts?: number[];
  medianMs: number;
  p99Ms: number;
}
export interface TailQuantileWindow {
  lowerMs: number;
  upperMs: number;
  lowerRank: number;
  upperRank: number;
}
export interface PropertyRelevance {
  reason: 'distribution' | 'tail';
  values: {
    value: string;
    count: number;
    p99Ms: number;
    p99Window: TailQuantileWindow | null;
  }[];
  cdfGap: number;
  cdfMargin: number;
  /** null when the winning comparison has a zero baseline or is body-only. */
  tailRatioLowerBound: number | null;
}
export interface PropertyScreen {
  examined: number;
  shown: number;
  limitedCoverage: number;
  limitedObservations: number;
  similarDistributions: number;
  pairsCompared: number;
  minimumSamples: number;
  minimumTailSamples: number;
  minimumCoverage: number;
  minimumCdfGap: number;
  minimumTailRatio: number;
  samplingBudget: number;
  interpretation: string;
}
export interface CategoricalHistogram {
  key: string;
  count: number;
  missingCount: number;
  binEdgesMs: number[];
  maxShare: number;
  values: CategoryLatency[];
  relevance?: PropertyRelevance | null;
}
export interface TailReport {
  count: number;
  typicalCount: number;
  slowCount: number;
  boundaryMs: number;
  medianMs: number;
  typicalElapsedMs: number | null;
  slowElapsedMs: number | null;
  typicalInsideMs: number | null;
  slowInsideMs: number | null;
  typicalOutsideMs: number | null;
  slowOutsideMs: number | null;
  typicalAttributed: number;
  slowAttributed: number;
  typicalComposition: TailComposition | null;
  slowComposition: TailComposition | null;
  signals: TailSignal[];
  groups: { key: string; values: string[]; typical: Record<string, number>; slow: Record<string, number> }[];
  histograms: CategoricalHistogram[];
  propertyScreen?: PropertyScreen;
  numericFieldsOmitted?: number;
  mixChanges: { key: string; value: string; typical: number; slow: number }[];
  examples: TailRecord[];
}

export interface TailResponse {
  report: TailReport;
  filesUsed: number;
  filesMatched: number;
  truncated: boolean;
  fieldsTruncated: boolean;
}
