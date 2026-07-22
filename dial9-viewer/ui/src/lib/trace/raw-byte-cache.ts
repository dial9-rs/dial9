// The raw-gzip byte cache: the LOWER level of the two-level segment cache.
//
// Evicting a parsed segment drops its heap, but re-entering that region should
// not re-download it. The compressed bytes stay here in a second, larger
// budget, so re-entry re-parses (~33 MB/s) instead of hitting S3 again.
//
// Split out of segments.ts, which owns the residency decisions that drive it.

import {
  BUDGET_EVICTION_THRESHOLD_FRACTION,
  RAW_GZIP_CACHE_BUDGET_BYTES,
  evictionTriggerBytes,
} from "./segment-budget.js";

// ── 4. Raw-gzip byte cache (two-level cache, lower level) ────────────────

/**
 * The raw-gzip byte cache: still-compressed segment bytes retained after
 * parse (and after parsed-data eviction) so re-entering a window
 * re-parses instead of re-downloading - the S3 GET churn killer. True
 * LRU by access, its OWN budget (256 MB default), same 90% trigger.
 */
export interface RawByteCache {
  /** Lookup + LRU touch. */
  get(key: string): Uint8Array | undefined;
  /** Lookup without the LRU touch (planning). */
  has(key: string): boolean;
  /**
   * Insert (LRU-evicting past the trigger). Returns false - NOT cached -
   * for entries larger than the trigger itself: caching one would evict
   * the entire cache and still exceed the trigger.
   */
  set(key: string, bytes: Uint8Array): boolean;
  delete(key: string): boolean;
  totalBytes(): number;
  /** Keys, least-recently-used first (test introspection). */
  keys(): string[];
}

export function createRawByteCache(
  budgetBytes: number = RAW_GZIP_CACHE_BUDGET_BYTES,
  thresholdFraction: number = BUDGET_EVICTION_THRESHOLD_FRACTION
): RawByteCache {
  const trigger = evictionTriggerBytes(budgetBytes, thresholdFraction);
  // Map iteration order is insertion order; re-inserting on access makes
  // the first key the least recently used.
  const entries = new Map<string, Uint8Array>();
  let total = 0;
  return {
    get(key) {
      const bytes = entries.get(key);
      if (bytes === undefined) return undefined;
      entries.delete(key);
      entries.set(key, bytes);
      return bytes;
    },
    has: (key) => entries.has(key),
    set(key, bytes) {
      const prev = entries.get(key);
      if (prev !== undefined) {
        entries.delete(key);
        total -= prev.byteLength;
      }
      if (bytes.byteLength > trigger) return false;
      entries.set(key, bytes);
      total += bytes.byteLength;
      for (const [oldKey, oldBytes] of entries) {
        if (total <= trigger) break;
        if (oldKey === key) continue; // never evict the just-inserted entry
        entries.delete(oldKey);
        total -= oldBytes.byteLength;
      }
      return true;
    },
    delete(key) {
      const bytes = entries.get(key);
      if (bytes === undefined) return false;
      entries.delete(key);
      total -= bytes.byteLength;
      return true;
    },
    totalBytes: () => total,
    keys: () => [...entries.keys()],
  };
}
