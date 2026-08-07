// Type declarations for `flamegraph_api.js` (pure helpers for the aggregated
// `?api=1` flamegraph mode: coverage badge/percent, UTC picker conversion,
// max_files ceiling, facet options). See src/types/decode.d.ts for the
// declaration-form rationale.
//
// Not frozen core, but loaded as a browser global (CommonJS-guard form) and
// consumed by typed src/ through the lib/trace boundary
// (src/lib/trace/api_format.ts) exactly like the core. Coverage-freeze and
// plateau/auto-stop helpers used to live here but were dropped when aggregation
// became a server-driven SSE stream; the coverage-signal port the src/ app
// still needs lives in lib/trace/aggregates.ts (typed Coverage).

declare module "*/flamegraph_api.js" {
  /**
   * The wire coverage block (see lib/trace/aggregates.ts Coverage). The
   * legacy helpers coerce every field through Number() and tolerate
   * missing fields, hence the loose shape here.
   */
  export interface LegacyCoverage {
    files_matched?: number;
    files_folded?: number;
    samples_folded?: number;
    total_bytes?: number;
    hosts_matched?: number;
    hosts_folded?: number;
  }

  /** `<option>` descriptor for the data-driven toolbar facet selects. */
  export interface FacetOption {
    value: string;
    label: string;
  }

  /**
   * "12 / 480 files (2.5%) [middot 8 / 40 hosts] middot 41,203 samples
   * [middot 4.1 MB]" - the stats-bar coverage badge. Host fraction omitted
   * unless hosts_matched > 1; bytes omitted when 0/absent.
   */
  export function formatCoverageBadge(coverage: LegacyCoverage): string;

  /** files_folded / files_matched * 100; 0 on a missing/zero denominator. */
  export function coveragePercent(
    coverage: LegacyCoverage | null | undefined
  ): number;

  /**
   * A "N files failed to fold" notice (with an example key when present), or
   * null when the coverage has no fold errors. Surfaces unwritable-output /
   * decode failures instead of silently rendering a shallow tree.
   */
  export function foldErrorNotice(
    coverage: (LegacyCoverage & { fold_errors?: number; fold_error_sample?: string }) | null | undefined
  ): string | null;

  /**
   * Millisecond input -> integer-ns STRING (the min_poll_ns/max_poll_ns scope
   * params are ns; the band inputs are human ms). null for empty/blank/
   * non-numeric/negative input. Fractional ms allowed (0.5 -> "500000").
   */
  export function msToNs(val: string | number | null | undefined): string | null;

  /**
   * Inverse of msToNs: epoch-ns -> millisecond string for seeding a band
   * input from a URL param. "" for null/empty/non-numeric; trailing zeros
   * trimmed (1_500_000 -> "1.5").
   */
  export function nsToMs(ns: string | number | null | undefined): string;

  /**
   * Next `max_files` ceiling for "Refine more": ~4x the current fold
   * count, at least `min` (default 16), capped (default 100000).
   */
  export function nextMaxFiles(
    currentFolded: number,
    opts?: { cap?: number; min?: number }
  ): number;

  /**
   * The depth `nextMaxFiles` should grow from on a "Refine more": the BOUNDED
   * WORK the server was allowed this request (`fold_work_cap`), not how many
   * cached files the snapshot happens to cover. An all-cache scope reports a
   * deep `files_folded` that would otherwise make one click jump straight to
   * the ceiling. Falls back to `currentMaxFiles`, then to `files_folded`, for
   * servers predating the cap.
   */
  export function refinementWorkDepth(
    coverage: (LegacyCoverage & { fold_work_cap?: number }) | null | undefined,
    currentMaxFiles: number | null
  ): number;

  /**
   * May a streamed snapshot replace the rendered tree? Refinement reconstructs
   * cached state from bounded seed batches, so a same-scope refine
   * (`preserveExisting`) must ignore snapshots shallower than the baseline
   * already on screen rather than momentarily shrinking it. A fresh scope
   * always adopts.
   */
  export function shouldAdoptRefinementSnapshot(
    preserveExisting: boolean,
    baselineFilesFolded: number,
    incomingFilesFolded: number
  ): boolean;

  /**
   * Epoch-ns -> `datetime-local` value ("YYYY-MM-DDTHH:MM:SS"), shown as
   * UTC wall-clock (S3 trace keys are bucketed in UTC). Empty string for
   * null/empty input. Accepts the string form for > 2^53 ns precision.
   */
  export function nsToPickerUtc(ns: number | string | null | undefined): string;

  /**
   * Inverse of nsToPickerUtc: `datetime-local` value -> epoch ns as a
   * STRING (precision), interpreting the picker value as UTC. Null for
   * empty input.
   */
  export function pickerUtcToNs(val: string | null | undefined): string | null;

  export function sourceFacetOptions(
    present: readonly string[] | null | undefined
  ): FacetOption[];

  export function threadFacetOptions(
    present: readonly string[] | null | undefined
  ): FacetOption[];

  /**
   * Host selector options: leading "All" (value "", label carries the
   * host count when > 1), then one option per host name.
   */
  export function hostFacetOptions(
    hostNames: readonly string[] | null | undefined
  ): FacetOption[];
}
