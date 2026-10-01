import type {
  Coverage,
  SpanDurationBucket,
  SpanTypeStats,
} from "../../lib/trace/index.js";

/** A baseline/filter pair known to cover exactly the same folded files. */
export interface MatchedHistogramPair {
  foldedSetId: string;
  baseline: readonly SpanDurationBucket[];
  filtered: readonly SpanDurationBucket[];
  filteredCount: number;
  filesFolded: number;
  filesMatched: number;
}

/**
 * Keep the last verified pair while either stream moves to a newer folded set.
 * Equal file counts are insufficient: the digests must match.
 */
export function retainMatchedHistogramPair(
  previous: MatchedHistogramPair | null,
  baseline: SpanTypeStats | undefined,
  baselineCoverage: Coverage | null,
  filtered: SpanTypeStats | null,
  filteredCoverage: Coverage | null,
  hasFilteredSnapshot: boolean,
): MatchedHistogramPair | null {
  const baselineSet = baselineCoverage?.folded_set_id ?? null;
  const filteredSet = filteredCoverage?.folded_set_id ?? null;
  if (
    !hasFilteredSnapshot ||
    baseline == null ||
    baselineSet == null ||
    filteredSet == null ||
    baselineSet !== filteredSet
  ) {
    return previous;
  }
  return {
    foldedSetId: baselineSet,
    baseline: baseline.histogram,
    filtered: filtered?.histogram ?? [],
    filteredCount: filtered?.count ?? 0,
    filesFolded: baselineCoverage.files_folded,
    filesMatched: baselineCoverage.files_matched,
  };
}
