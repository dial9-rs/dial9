// Level-of-detail index for timeline tracks (the "mipmap"). Ported from the
// implicit segment tree Perfetto uses (`ImplicitSegmentForest`, itself from
// Tristan Hume's `gigatrace`). One O(n) build answers a range-aggregate query
// over any contiguous leaf range in O(log n), for ANY zoom resolution - so a
// zoomed-out track never scans every span; it asks the forest for one
// representative per pixel-column.
//
// Generic over the aggregate: pass a COMMUTATIVE, ASSOCIATIVE merge. For the
// span mipmap the leaves are spans sorted by start-ts and the merge keeps the
// longest span and sums the count (see `spanBucketMerge`), so each pixel-column
// renders one rect (the longest) while density (count) is preserved.

export type Merge<T> = (a: T, b: T) => T;

export class SegmentForest<T> {
  private readonly tree: T[];
  private readonly n: number;
  private readonly merge: Merge<T>;

  /** Build over `leaves` (typically sorted by ts). O(n) time + space. */
  constructor(leaves: readonly T[], merge: Merge<T>) {
    const n = leaves.length;
    this.n = n;
    this.merge = merge;
    if (n === 0) {
      this.tree = [];
      return;
    }
    // Iterative segment tree: leaves at [n, 2n), internal node i = merge(2i, 2i+1).
    const tree = new Array<T>(2 * n);
    for (let i = 0; i < n; i++) tree[n + i] = leaves[i]!;
    for (let i = n - 1; i >= 1; i--) tree[i] = merge(tree[2 * i]!, tree[2 * i + 1]!);
    this.tree = tree;
  }

  get size(): number {
    return this.n;
  }

  /**
   * Aggregate over the half-open leaf range [lo, hi). Returns null for an empty
   * range. Correct for a commutative merge (left/right partials combined in any
   * order); max-dur + sum-count is commutative.
   */
  query(lo: number, hi: number): T | null {
    if (lo < 0) lo = 0;
    if (hi > this.n) hi = this.n;
    if (lo >= hi) return null;
    const tree = this.tree;
    const merge = this.merge;
    let acc: T | null = null;
    let l = lo + this.n;
    let r = hi + this.n;
    while (l < r) {
      if (l & 1) { acc = acc === null ? tree[l]! : merge(acc, tree[l]!); l++; }
      if (r & 1) { r--; acc = acc === null ? tree[r]! : merge(acc, tree[r]!); }
      l >>= 1;
      r >>= 1;
    }
    return acc;
  }
}

/** One mipmap bucket's representative: the longest span + summed count. `idx`
 * points back into the source span columns (for exact hover/click). */
export interface SpanAgg {
  dur: number;
  count: number;
  idx: number;
}

/** Perfetto's rule: keep the longer span as the visual representative, sum the
 * counts so density survives even though one rect is drawn. Ties break to the
 * EARLIER idx so the result is independent of merge order (the segment tree
 * combines partials in an implementation-defined order) - i.e. truly
 * commutative + deterministic. */
export const spanBucketMerge: Merge<SpanAgg> = (a, b) => {
  const count = a.count + b.count;
  if (a.dur > b.dur) return { dur: a.dur, count, idx: a.idx };
  if (b.dur > a.dur) return { dur: b.dur, count, idx: b.idx };
  return { dur: a.dur, count, idx: a.idx < b.idx ? a.idx : b.idx };
};
