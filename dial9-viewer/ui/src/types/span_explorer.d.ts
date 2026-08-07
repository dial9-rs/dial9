// Type declarations for `span_explorer.js` (DOM-free helpers for the Span
// Explorer page: catalog sorting/labelling, log-duration histogram geometry,
// percentile estimation, five-way time composition, attribute filters, and the
// flamegraph/viewer deep-link builders). See src/types/decode.d.ts for the
// declaration-form rationale.
//
// Loaded as a browser global in CommonJS-guard form and consumed by typed src/
// only through the lib/trace boundary (src/lib/trace/span_explorer.ts).
//
// The wire shapes (SpanTypeStats, Exemplar, ...) are declared once in
// src/lib/trace/aggregates.ts; this module re-states only the LOOSE shapes the
// frozen helpers actually accept, since they coerce every numeric field through
// Number() and tolerate missing ones (old traces, older servers).

declare module "*/span_explorer.js" {
  /** A histogram bar as the helpers accept it: fields may arrive as strings. */
  export interface HistogramBarLike {
    lo_ns: number | string;
    hi_ns: number | string;
    count: number | string;
  }

  /** A normalized bar: every field coerced, ascending by `lo_ns`. */
  export interface SpanHistogramBar {
    lo_ns: number;
    hi_ns: number;
    count: number;
  }

  /** One laid-out histogram column, in pixels within the drawn width. */
  export interface SpanHistogramColumn extends SpanHistogramBar {
    x: number;
    w: number;
    /** Height as a fraction of the tallest bar (0..1). */
    hFrac: number;
  }

  export interface SpanHistogramLayout {
    cols: SpanHistogramColumn[];
    maxCount: number;
  }

  /** An inclusive duration band; either end may be open. */
  export interface DurationBand {
    min_ns: number | null;
    max_ns: number | null;
  }

  /** One of the five composition categories, resolved for display. */
  export interface CompositionCategory {
    key: "on_cpu" | "blocked" | "async_wait" | "sched_delay" | "unknown";
    label: string;
    /** Hex swatch, fixed per category. */
    color: string;
    /** Total time in this category across instances. */
    ns: number;
    /** Share of the bar (0..1); see `weighting` for what it means. */
    frac: number;
    /** Mean time per instance, null when the instance count is unknown. */
    meanNs: number | null;
  }

  export interface TimeCompositionView {
    total_ns: number;
    instance_count: number;
    /**
     * "equal" - each instance contributes equally (the backend supplied
     * per-instance fraction sums); "time" - shares are ns-weighted, so long
     * instances dominate. The UI must label which one it is showing.
     */
    weighting: "equal" | "time";
    categories: CompositionCategory[];
  }

  /** The accumulated raw sums for a duration band; see bandComposition. */
  export interface BandCompositionSums {
    on_cpu_ns: number;
    blocked_ns: number;
    async_wait_ns: number;
    scheduler_delay_ns: number;
    unknown_ns: number;
    instance_count: number;
    on_cpu_frac_sum: number;
    blocked_frac_sum: number;
    async_wait_frac_sum: number;
    scheduler_delay_frac_sum: number;
    unknown_frac_sum: number;
  }

  /** One `attr=key=value` filter. Multiple filters are ANDed. */
  export interface AttrFilter {
    key: string;
    value: string;
  }

  /** The page scope a deep link is built against. */
  export interface SpanExplorerState {
    data_dir?: string | null;
    max_files?: number | null;
    bucket?: string | null;
    region?: string | null;
    credentialMode?: "ambient" | "literal" | "role" | null;
    roleArn?: string | null;
    prefix?: string | null;
    service?: string | null;
    hosts?: readonly string[] | null;
    start_ns?: string | null;
    end_ns?: string | null;
    span_type_uid?: string | null;
    min_span_ns?: number | null;
    max_span_ns?: number | null;
  }

  /** decodeSpanExplorerState always fills every key (null when absent). */
  export interface DecodedSpanExplorerState {
    data_dir: string | null;
    max_files: number | null;
    bucket: string | null;
    region: string | null;
    credentialMode: string | null;
    roleArn: string | null;
    prefix: string | null;
    service: string | null;
    hosts: string[];
    start_ns: string | null;
    end_ns: string | null;
    span_type_uid: string | null;
    min_span_ns: number | null;
    max_span_ns: number | null;
  }

  /** Scope for exemplarViewerUrl; the exemplar's own host wins over any here. */
  export interface ExemplarLinkScope {
    /** Original source URL in raw mode; avoids requiring a backend source key. */
    trace?: string | null;
    bucket?: string | null;
    region?: string | null;
    credentialMode?: "ambient" | "literal" | "role" | null;
    roleArn?: string | null;
    service?: string | null;
    /** Forwarded as `focus_span_name` so the viewer selects the exact span. */
    spanName?: string | null;
  }

  /** Which stream a snapshot arrived on. */
  export type StreamMode = "replace" | "refine" | "exemplars";

  // ── Query-param plumbing ──

  /** Set or delete `max_files` so API and browser URLs cannot drift. */
  export function setMaxFilesParam(
    searchParams: URLSearchParams,
    maxFiles: number | null | undefined,
  ): URLSearchParams;

  /**
   * May a streamed full snapshot replace the visible catalog? Refinement
   * reconstructs cached state from bounded seed batches, so a `refine` snapshot
   * below the visible baseline must not momentarily shrink the page, and an
   * `exemplars` stream never replaces the catalog at all.
   */
  export function shouldAdoptCatalogSnapshot(
    mode: StreamMode | null | undefined,
    baselineFilesFolded: number | null | undefined,
    incomingFilesFolded: number | null | undefined,
  ): boolean;

  // ── Formatting ──

  /** Duration to a compact unit-suffixed string; "—" for null/invalid. */
  export function fmtNs(ns: number | string | null | undefined): string;

  /** Percentile rank (0..100) to "pNN"/"pNN.N"; keeps tail precision. */
  export function fmtPercentile(p: number | null | undefined): string;

  // ── Catalog ──

  /** Sort a copy of `types` by `key`; descending unless `ascending`. */
  export function sortSpanTypes<T>(
    types: readonly T[],
    key: keyof T & string,
    ascending?: boolean,
  ): T[];

  /** Display label: span name plus a `(file:line)` callsite when known. */
  export function spanTypeLabel(st: {
    name?: string | null;
    callsite_file?: string | null;
    callsite_line?: number | null;
  }): string;

  /** Fraction of instances with full composition data; null when unknown. */
  export function spanTypeQuality(spanType: {
    details_complete_count?: number | null;
    partial_count?: number | null;
  }): number | null;

  // ── Histogram geometry ──

  export function normalizeSpanHistogram(
    hist: readonly HistogramBarLike[] | null | undefined,
  ): SpanHistogramBar[];

  export function spanHistogramLayout(
    bars: readonly HistogramBarLike[] | null | undefined,
    width: number,
    gap?: number,
  ): SpanHistogramLayout;

  /** Pixel x to duration, geometric within the containing column. */
  export function spanPxToNs(
    bars: readonly HistogramBarLike[] | null | undefined,
    width: number,
    px: number,
  ): number | null;

  /** Inverse of spanPxToNs; clamps outside the histogram's range. */
  export function spanNsToPx(
    bars: readonly HistogramBarLike[] | null | undefined,
    width: number,
    ns: number | null | undefined,
  ): number | null;

  /** Brushed pixel range to a band; null for a drag under 2px (a click). */
  export function spanBrushToBand(
    bars: readonly HistogramBarLike[] | null | undefined,
    width: number,
    x0: number,
    x1: number,
  ): DurationBand | null;

  /** Duration at percentile `p` (0..100), estimated from the bar counts. */
  export function durationAtPercentile(
    bars: readonly HistogramBarLike[] | null | undefined,
    p: number,
  ): number | null;

  /** Instances whose bar overlaps the band. Either bound may be null. */
  export function countInBand(
    bars: readonly HistogramBarLike[] | null | undefined,
    minNs: number | null,
    maxNs: number | null,
  ): number;

  /** Percentile rank (0..100) of `ns` within the distribution. */
  export function percentileForDuration(
    bars: readonly HistogramBarLike[] | null | undefined,
    ns: number | null | undefined,
  ): number | null;

  // ── Time composition ──

  /** The five categories in display order, with their fixed swatches. */
  export const TIME_CATEGORIES: readonly {
    key: CompositionCategory["key"];
    label: string;
    color: string;
  }[];

  /** Sum the `composition_histogram` buckets inside a band; null when absent. */
  export function bandComposition(
    spanType: { composition_histogram?: readonly unknown[] },
    min_ns: number | null,
    max_ns: number | null,
  ): BandCompositionSums | null;

  /**
   * Resolve a span type's composition for display, scoped to `band` when the
   * backend supplied per-bucket data. Falls back to a single histogram-estimated
   * "Unknown" bar when there is no composition at all.
   */
  export function computeTimeComposition(
    spanType: {
      composition?: unknown;
      composition_histogram?: readonly unknown[];
      histogram?: readonly HistogramBarLike[];
    },
    band?: DurationBand | null,
  ): TimeCompositionView;

  // ── Exemplars ──

  /** Union of attribute keys across exemplars, in first-seen order. */
  export function collectExemplarAttributeKeys(
    exemplars: readonly { attributes?: readonly { key?: string }[] }[] | null | undefined,
  ): string[];

  /** An exemplar's value for an attribute key, or null when absent. */
  export function exemplarAttrValue(
    ex: { attributes?: readonly { key?: string; value?: string }[] } | null | undefined,
    key: string,
  ): string | null;

  /**
   * Does a column carry no distinguishing information — every row maps to the
   * same value, or all are empty? False for fewer than 2 rows: one row cannot
   * establish uniformity, and a 1-row table must not hide its columns.
   */
  export function columnIsDegenerate<T>(
    rows: readonly T[] | null | undefined,
    valueOf: (row: T) => string | number | null | undefined,
  ): boolean;

  /** Exemplars whose `elapsed_ns` falls inside the inclusive bounds. */
  export function exemplarsInBand<T extends { elapsed_ns?: number }>(
    exemplars: readonly T[] | null | undefined,
    minNs: number | null,
    maxNs: number | null,
  ): T[];

  /**
   * Patch ONLY the selected type's exemplar fields into the visible catalog.
   * A duration-bounded refresh must not replace catalog statistics with an
   * early partial snapshot, nor rerender unrelated rows.
   */
  export function mergeSelectedExemplarSnapshot<T extends { span_type_uid?: string }>(
    currentTypes: readonly T[] | null | undefined,
    incomingTypes: readonly T[] | null | undefined,
    selectedUid: string | null,
  ): { spanTypes: T[]; matched: boolean };

  /**
   * How to treat one cumulative snapshot of a seed-only exemplar stream.
   * `complete` when it represents exactly the catalog's folded set; `preview`
   * additionally when it is a subset working toward that same target.
   */
  export function classifyExemplarSnapshot(
    baselineSetId: string | null | undefined,
    currentSetId: string | null | undefined,
    targetSetId: string | null | undefined,
  ): { preview: boolean; complete: boolean };

  export function completeExemplarRefresh<T, C>(
    currentTypes: T,
    currentCoverage: C,
    snapshotAdopted: boolean,
  ): { spanTypes: T; coverage: C; pending: boolean };

  /**
   * Compare catalogs ignoring the duration-scoped exemplar fields, so a
   * preserve-mode refresh can skip rerendering an already-complete catalog
   * while still adopting a final snapshot after an interrupted refinement.
   */
  export function sameSpanCatalogStatistics(
    leftTypes: readonly unknown[] | null | undefined,
    rightTypes: readonly unknown[] | null | undefined,
  ): boolean;

  /**
   * Is a preserve-mode response still valid? Rejects a late A:X response after
   * an A -> B -> A switch has returned the UI to A:global.
   */
  export function exemplarRequestMatches(
    requestUid: string | null,
    requestScopeKey: string,
    currentUid: string | null,
    currentScopeKey: string,
  ): boolean;

  // ── Attribute filters ──

  /** Parse repeated `attr=key=value` params; splits on the FIRST `=`. */
  export function parseAttrFilterParams(
    rawList: readonly string[] | null | undefined,
  ): AttrFilter[];

  /** Serialize filters back to `key=value` strings. */
  export function formatAttrFilterParams(
    filters: readonly AttrFilter[] | null | undefined,
  ): string[];

  export function hasAttrFilter(
    filters: readonly AttrFilter[] | null | undefined,
    key: string,
    value: string,
  ): boolean;

  /** Append the pair if absent; always returns a new array. */
  export function addAttrFilter(
    filters: readonly AttrFilter[] | null | undefined,
    key: string,
    value: string,
  ): AttrFilter[];

  export function removeAttrFilter(
    filters: readonly AttrFilter[] | null | undefined,
    key: string,
    value: string,
  ): AttrFilter[];

  // ── Deep links ──

  export function encodeSpanExplorerState(state: SpanExplorerState): URLSearchParams;

  export function decodeSpanExplorerState(params: URLSearchParams): DecodedSpanExplorerState;

  /**
   * Flamegraph URL for the selected span type + band. `phase` "blocking"
   * selects the sched sample source; anything else selects cpu.
   */
  export function flamegraphUrl(
    state: SpanExplorerState,
    phase?: "blocking" | "cpu" | null,
  ): string;

  /**
   * Viewer deep link onto one exemplar: the raw trace or an `/api/object`
   * component plus NON-DESTRUCTIVE `focus_*` params. Never emits `start`/`end`,
   * which the viewer treats as a hard parse filter that would drop every
   * surrounding event. Empty string when neither source is available.
   */
  export function exemplarViewerUrl(
    exemplar:
      | {
          source_key?: string;
          host?: string;
          start_ns?: number | string;
          end_ns?: number | string;
        }
      | null
      | undefined,
    scope: ExemplarLinkScope | null | undefined,
  ): string;

}
