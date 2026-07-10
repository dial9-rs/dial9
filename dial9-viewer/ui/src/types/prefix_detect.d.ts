// Hand-written type declarations for the frozen-core file `prefix_detect.js`
// (S3 prefix-discovery heuristics). See src/types/decode.d.ts for the
// declaration-form rationale. Verified against prefix_detect.js and the
// index.html call site (isDateLayer(prefixes) gating the date layer).

declare module "*/prefix_detect.js" {
  /**
   * Last non-empty path segment of an S3 prefix.
   * "traces/2026-06-12/" -> "2026-06-12".
   */
  export function lastSegment(prefix: string): string;

  /**
   * Whether a listing's children are all date partitions (YYYY-MM-DD/)
   * rather than genuine key prefixes (issue #471). Empty/missing input is
   * not a date layer.
   */
  export function isDateLayer(
    prefixes: readonly string[] | null | undefined
  ): boolean;
}
