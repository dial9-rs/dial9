// Hand-written type declarations for the frozen-core file `heatmap.js`
// (S3 browser density timeline helpers). See src/types/decode.d.ts for the
// declaration-form rationale.
// Verified against heatmap.js and the index.html call sites (normalized
// segments are built there as {key, size, start, end, service, host, bootId}).

declare module "*/heatmap.js" {
  /** Max total bytes allowed to open in the viewer at once (open-size cap). */
  export const MAX_OPEN_BYTES: number;

  /** Minimum seconds attributed to a segment with an unknown/degenerate end. */
  export const MIN_SEGMENT_SECONDS: number;

  /**
   * Minimal segment shape the helpers read. index.html's normalized
   * segments carry more fields (key, ...); the generics below preserve
   * them. `service`/`host`/`bootId` come from parseKey and may be missing
   * on unknown key layouts. Times are epoch seconds.
   */
  export interface SegmentInput {
    start: number;
    /** May be missing/degenerate; segmentSpan() floors the span. */
    end?: number | null;
    /** Byte size; treated as 0 when missing. */
    size?: number | null;
    service?: string | null;
    host?: string | null;
    bootId?: string | null;
  }

  /** Normalize a segment's [start, end] span in seconds (end > start). */
  export function segmentSpan(seg: SegmentInput): {
    start: number;
    end: number;
  };

  export interface HostRow<S extends SegmentInput = SegmentInput> {
    service: string;
    host: string;
    label: string;
    /** Sorted by start time. */
    segments: S[];
    totalBytes: number;
  }

  /**
   * Group segments into one row per service/host, rows sorted by label.
   * Boot transitions do NOT split rows (see bootTransitions).
   */
  export function groupByHost<S extends SegmentInput>(
    segments: readonly S[]
  ): HostRow<S>[];

  /** Boot-id transitions within a row's segments (input need not be sorted). */
  export function bootTransitions(
    rowSegments: readonly SegmentInput[]
  ): { time: number; fromBoot: string; toBoot: string }[];

  /**
   * Density-rendering copies with each end clamped to the next start so
   * consecutive segments tile instead of double-counting at the seam.
   * The original end is preserved on `realEnd`.
   */
  export function tileSegments<S extends SegmentInput>(
    rowSegments: readonly S[]
  ): (S & { realEnd: S["end"] })[];

  /** Genuine coverage gaps within a row, [{start, end}] seconds, sorted. */
  export function segmentGaps(
    rowSegments: readonly SegmentInput[]
  ): { start: number; end: number }[];

  /**
   * Bytes-per-pixel-column over [t0, t1] seconds; each segment's bytes are
   * spread uniformly across its span. Length-`width` Float64Array.
   */
  export function accumulateDensity(
    segments: readonly SegmentInput[],
    t0: number,
    t1: number,
    width: number
  ): Float64Array;

  /**
   * Segments whose [start, end) span touches [t0, t1] (start-inclusive,
   * end-exclusive).
   */
  export function segmentsOverlapping<S extends SegmentInput>(
    segments: readonly S[],
    t0: number,
    t1: number
  ): S[];

  /** Total byte size of a set of segments (for the open-size cap). */
  export function totalBytes(segments: readonly SegmentInput[]): number;

  /**
   * Map a normalized density in [0, 1] to a CSS color (dim blue -> purple
   * -> red -> yellow; 0 = page background).
   */
  export function densityColor(norm: number): string;
}
