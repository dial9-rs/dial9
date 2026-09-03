/** Target histogram resolution: one time bucket for roughly every 8 CSS px. */
export const SPAWN_BIN_TARGET_PX = 8;

const ONE_SECOND_NS = 1_000_000_000;

/** Spawn counts in viewport-adaptive, fixed-pixel-width time buckets. */
export interface SpawnHistogramModel {
  /** Spawn count per bucket, left-to-right across the viewport. */
  counts: readonly number[];
  /** Peak count across the visible buckets, >= 1. */
  maxSpawns: number;
  /** Peak count in any actual one-second window across visible spawns. */
  peakSpawnsPerSecond: number;
  /** Width of one bucket in CSS px. */
  binWidthPx: number;
  /** Duration represented by one bucket in trace nanoseconds. */
  binDurationNs: number;
}

/** One nonempty spawn-histogram bin at a cursor time. */
export interface SpawnHistogramBin {
  index: number;
  count: number;
  /** Left edge of the displayed time quantum. */
  startNs: number;
  /** Right edge of the displayed time quantum. */
  endNs: number;
  /** Inclusive end used by the existing spawned-task range selection. */
  selectionEndNs: number;
  durationNs: number;
  ratePerSecond: number;
}

/** Binary search: index of the first number >= target. */
function lowerBound(arr: readonly number[], target: number): number {
  let lo = 0;
  let hi = arr.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (arr[mid]! < target) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/** Binary search: index of the first number > target. */
function upperBound(arr: readonly number[], target: number): number {
  let lo = 0;
  let hi = arr.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (arr[mid]! <= target) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

function geometry(
  viewStart: number,
  viewEnd: number,
  drawW: number,
): { numBins: number; binWidthPx: number; binDurationNs: number } | null {
  const viewDur = viewEnd - viewStart;
  if (viewDur <= 0 || drawW <= 0) return null;
  const numBins = Math.max(1, Math.ceil(drawW / SPAWN_BIN_TARGET_PX));
  return {
    numBins,
    binWidthPx: drawW / numBins,
    binDurationNs: viewDur / numBins,
  };
}

/**
 * Count sorted task-spawn timestamps into viewport-adaptive time buckets. The
 * independent scale preserves burst shape without flattening a task-total line.
 */
export function buildSpawnHistogram(
  spawnTimes: readonly number[],
  viewStart: number,
  viewEnd: number,
  drawW: number,
): SpawnHistogramModel | null {
  if (spawnTimes.length === 0) return null;
  const geom = geometry(viewStart, viewEnd, drawW);
  if (geom === null) return null;

  const counts = new Array<number>(geom.numBins).fill(0);
  let maxSpawns = 0;
  const from = lowerBound(spawnTimes, viewStart);
  const to = upperBound(spawnTimes, viewEnd);
  let peakSpawnsPerSecond = 0;
  let windowStart = from;
  for (let i = from; i < to; i++) {
    const t = spawnTimes[i]!;
    const bin = Math.min(
      geom.numBins - 1,
      Math.floor(((t - viewStart) / (viewEnd - viewStart)) * geom.numBins),
    );
    const count = counts[bin]! + 1;
    counts[bin] = count;
    if (count > maxSpawns) maxSpawns = count;

    while (spawnTimes[windowStart]! <= t - ONE_SECOND_NS) {
      windowStart++;
    }
    peakSpawnsPerSecond = Math.max(peakSpawnsPerSecond, i - windowStart + 1);
  }
  if (maxSpawns === 0) return null;
  return {
    counts,
    maxSpawns,
    peakSpawnsPerSecond,
    binWidthPx: geom.binWidthPx,
    binDurationNs: geom.binDurationNs,
  };
}

/**
 * Resolve a cursor timestamp directly against sorted spawn times. Histogram
 * bins are half-open, except the final bin includes viewEnd.
 */
export function spawnHistogramBinAtTimes(
  spawnTimes: readonly number[],
  viewStart: number,
  viewEnd: number,
  drawW: number,
  ns: number,
): SpawnHistogramBin | null {
  const geom = geometry(viewStart, viewEnd, drawW);
  if (
    geom === null ||
    spawnTimes.length === 0 ||
    ns < viewStart ||
    ns > viewEnd
  ) {
    return null;
  }
  const lastIndex = geom.numBins - 1;
  const index =
    ns === viewEnd
      ? lastIndex
      : Math.min(
          lastIndex,
          Math.floor(((ns - viewStart) / (viewEnd - viewStart)) * geom.numBins),
        );
  const startNs = viewStart + index * geom.binDurationNs;
  const endNs =
    index === lastIndex
      ? viewEnd
      : Math.min(viewEnd, viewStart + (index + 1) * geom.binDurationNs);
  const from = lowerBound(spawnTimes, startNs);
  const to =
    index === lastIndex
      ? upperBound(spawnTimes, endNs)
      : lowerBound(spawnTimes, endNs);
  const count = to - from;
  if (count === 0) return null;
  return {
    index,
    count,
    startNs,
    endNs,
    selectionEndNs:
      index === lastIndex ? endNs : Math.max(startNs, Math.ceil(endNs) - 1),
    durationNs: endNs - startNs,
    ratePerSecond: (count * 1_000_000_000) / (endNs - startNs),
  };
}

/** Resolve a cursor against an already-built histogram model. */
export function spawnHistogramBinAt(
  histogram: SpawnHistogramModel | null,
  viewStart: number,
  viewEnd: number,
  ns: number,
): SpawnHistogramBin | null {
  if (
    histogram === null ||
    histogram.counts.length === 0 ||
    viewEnd <= viewStart ||
    ns < viewStart ||
    ns > viewEnd
  ) {
    return null;
  }
  const lastIndex = histogram.counts.length - 1;
  const index =
    ns === viewEnd
      ? lastIndex
      : Math.min(
          lastIndex,
          Math.floor(((ns - viewStart) / (viewEnd - viewStart)) * histogram.counts.length),
        );
  const count = histogram.counts[index]!;
  if (count === 0) return null;
  const startNs = viewStart + index * histogram.binDurationNs;
  const endNs =
    index === lastIndex
      ? viewEnd
      : Math.min(viewEnd, viewStart + (index + 1) * histogram.binDurationNs);
  return {
    index,
    count,
    startNs,
    endNs,
    selectionEndNs:
      index === lastIndex ? endNs : Math.max(startNs, Math.ceil(endNs) - 1),
    durationNs: endNs - startNs,
    ratePerSecond: (count * 1_000_000_000) / (endNs - startNs),
  };
}

/** Human-readable task-spawn rate with an explicit normalized unit. */
export function formatSpawnRate(ratePerSecond: number): string {
  if (!Number.isFinite(ratePerSecond) || ratePerSecond <= 0) return "0 tasks/s";
  if (ratePerSecond < 1) return `${ratePerSecond.toFixed(2)} tasks/s`;
  return `${Math.round(ratePerSecond)} tasks/s`;
}
