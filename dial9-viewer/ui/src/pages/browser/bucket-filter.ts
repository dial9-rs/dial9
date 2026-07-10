// Bucket-picker filter predicate (T15 amendment of features/01 C6;
// ADR-0004 section 1, Live-validation Finding 2).
//
// The legacy page hardcoded the picker's trace-bucket predicate to names
// containing "dial9" - any differently-named bucket was invisible until
// #607 bolted on a "Show all" toggle. The migrated page makes the
// predicate config-driven instead:
//
//   1. page-URL override:  ?bucket_filter=<substring>   (wins; "" allowed)
//   2. server config:      /api/config `bucket_filter`  (server default "dial9")
//   3. client fallback:    "dial9"                      (old servers without
//                                                        the field)
//
// An empty filter matches every bucket (filtering disabled: the filtered
// and full lists coincide, so the show-all toggle disappears). Matching is
// case-insensitive substring, exactly like the legacy predicate.
//
// Pure module (no DOM/store) so the resolution and predicate are
// unit-testable under the node test env.

/** The fallback when neither URL nor server provides a filter. */
export const DEFAULT_BUCKET_FILTER = "dial9";

/** Case-insensitive substring predicate ("" matches everything). */
export function bucketMatchesFilter(name: string, filter: string): boolean {
  return name.toLowerCase().includes(filter.toLowerCase());
}

export interface ResolvedBucketFilter {
  /** The filter in effect at page load (before /api/config resolves). */
  filter: string;
  /**
   * The page-URL `bucket_filter=` value, or null when absent. Non-null
   * beats the server config value and must be carried through every URL
   * sync so the override survives history.replaceState.
   */
  override: string | null;
}

/** Resolve the page-load filter state from the page URL's query string. */
export function resolveBucketFilter(search: string): ResolvedBucketFilter {
  const override = new URLSearchParams(search).get("bucket_filter");
  return {
    filter: override ?? DEFAULT_BUCKET_FILTER,
    override,
  };
}
