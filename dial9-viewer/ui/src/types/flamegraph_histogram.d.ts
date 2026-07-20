// Type declarations for `flamegraph_histogram.js` - the pure poll-duration
// histogram minimap helpers (aggregated `?api=1` flamegraph mode). See
// src/types/decode.d.ts for the declaration-form rationale.
//
// Not frozen core, but loaded as a browser global (CommonJS-guard form) and
// consumed by typed src/ through the lib/canvas boundary
// (src/lib/canvas/flamegraph_histogram.ts) exactly like the core.

declare module "*/flamegraph_histogram.js" {
  /** One log2 poll-duration histogram bucket (ascending by range). */
  export interface PollHistogramBar {
    lo_ns: number;
    hi_ns: number;
    samples: number;
  }

  /** A laid-out column: a bar plus its pixel x/width and height fraction. */
  export interface HistogramColumn extends PollHistogramBar {
    /** Left edge, px. */
    x: number;
    /** Column width, px (minus the inter-column gap). */
    w: number;
    /** Height fraction of the tallest bar (0..1). */
    hFrac: number;
  }

  /** Result of histogramLayout: equal-width columns + the peak sample count. */
  export interface HistogramLayout {
    cols: HistogramColumn[];
    maxSamples: number;
  }

  /** A continuous poll-duration band (nanoseconds). */
  export interface PollBand {
    min_ns: number;
    max_ns: number;
  }

  /**
   * Format a nanosecond duration as a short axis/tooltip label
   * ("500µs", "1.5ms", "2s"). Empty string for non-finite/negative input.
   */
  export function fmtDurationNs(ns: number | string): string;

  /**
   * Validate + sort the backend histogram ascending by `lo_ns`, dropping
   * malformed entries (non-finite / negative / hi<=lo). Non-array -> [].
   */
  export function normalizeHistogram(hist: unknown): PollHistogramBar[];

  /**
   * The sample-weighted median poll duration (ns) - the `lo_ns` of the bar
   * crossing the halfway sample count. null for an empty histogram.
   */
  export function sampleWeightedMedianNs(bars: readonly PollHistogramBar[]): number | null;

  /**
   * Equal-width column geometry for a `width`-px strip, `gap` px between
   * columns (default 1). Empty bars / non-positive width -> no columns.
   */
  export function histogramLayout(
    bars: readonly PollHistogramBar[],
    width: number,
    gap?: number
  ): HistogramLayout;

  /**
   * Map a pixel x over the strip to a CONTINUOUS poll duration (ns) via
   * geometric (log-linear) interpolation within the column under `px`.
   * `cols` may be passed to avoid recomputing. null when there are no bars.
   */
  export function pxToNs(
    bars: readonly PollHistogramBar[],
    width: number,
    px: number,
    cols?: readonly HistogramColumn[] | null
  ): number | null;

  /**
   * Map a brushed pixel range [x0, x1] to a continuous { min_ns, max_ns }
   * band. null for no bars or a near-zero-width drag (treated as a click).
   */
  export function brushToBand(
    bars: readonly PollHistogramBar[],
    width: number,
    x0: number,
    x1: number
  ): PollBand | null;
}
