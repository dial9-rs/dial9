// Type declarations for `flamegraph_api.js` (pure helpers for the aggregated
// `?api=1` flamegraph mode: coverage badge/percent, auto-stop plateau
// heuristic, UTC picker conversion, max_files ceiling, facet options).
// See src/types/decode.d.ts for the declaration-form rationale.
//
// Not frozen core, but loaded as a browser global (CommonJS-guard form) and
// consumed by typed src/ through the lib/trace boundary
// (src/lib/trace/api_format.ts) exactly like the core. `isCoverageFrozen` is
// deliberately NOT re-exported into src/: the typed port in
// lib/trace/aggregates.ts (same semantics, typed Coverage) is the src/-side
// implementation.

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

  export function isCoverageFrozen(
    prev: LegacyCoverage | null | undefined,
    curr: LegacyCoverage
  ): boolean;

  /**
   * True once the last `patience` (default 3) per-poll coverage gains
   * (percentage points, newest last) are all below `minDeltaPct`
   * (default 0.5) - the diminishing-returns auto-stop.
   */
  export function shouldAutoStopRefining(
    deltas: readonly number[],
    opts?: { minDeltaPct?: number; patience?: number }
  ): boolean;

  /**
   * Next `max_files` ceiling for "Refine more": ~4x the current fold
   * count, at least `min` (default 16), capped (default 100000).
   */
  export function nextMaxFiles(
    currentFolded: number,
    opts?: { cap?: number; min?: number }
  ): number;

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
