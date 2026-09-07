// Type declarations for the frozen-core file `heatmap.js`
// (S3 browser density timeline helpers). See src/types/decode.d.ts for the
// declaration-form rationale.

declare module "*/heatmap.js" {
  /** Max total bytes allowed to open in the viewer at once (open-size cap). */
  export const MAX_OPEN_BYTES: number;

  /** Minimum seconds attributed to a segment with an unknown/degenerate end. */
  export const MIN_SEGMENT_SECONDS: number;

  /**
   * Minimal segment shape the helpers read. Callers' normalized segments
   * carry more fields (key, ...); the generics below preserve them.
   * `service`/`host`/`bootId` come from parseKey and may be missing on
   * unknown key layouts. Times are epoch seconds.
   *
   * The optional fields include `| undefined` explicitly
   * (exactOptionalPropertyTypes): the JS treats an explicitly-undefined
   * field the same as a missing one, and callers' normalized segments do
   * carry explicit undefineds from parseKey.
   */
  export interface SegmentInput {
    start: number;
    /** May be missing/degenerate; segmentSpan() floors the span. */
    end?: number | null | undefined;
    /** Byte size; treated as 0 when missing. */
    size?: number | null | undefined;
    service?: string | null | undefined;
    host?: string | null | undefined;
    bootId?: string | null | undefined;
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

  /**
   * "Nice" axis tick times for the epoch-second range [tMin, tMax], at most
   * `targetCount` of them. Ticks snap to human intervals (1/5/10/30s,
   * 1/2/5/10/15/30m, 1/2/3/6/12h, 1/2/7d) and are aligned to multiples of the
   * chosen step, so labels land on round wall-clock times. A degenerate range
   * (tMax <= tMin) yields a single tick at tMin.
   */
  export function niceTimeTicks(
    tMin: number,
    tMax: number,
    targetCount: number
  ): number[];

  /**
   * Whether a document-level click should clear the current browse
   * selection. Control surfaces preserve it: the timeline itself, the
   * actions bar, and the page header (the TZ toggle only relabels the
   * axis). `wasDrag` suppresses the synthetic click that trails a
   * selection drag ending outside the pane.
   *
   * Every flag is required so a caller cannot silently omit one and
   * reintroduce the clicks-that-should-not-clear bugs (#644, #645).
   */
  export function shouldClearSelectionOnClick(o: {
    isBrowseTab: boolean;
    hasSelection: boolean;
    wasDrag: boolean;
    targetInHeatmap: boolean;
    targetInActions: boolean;
    targetInHeader: boolean;
  }): boolean;
}
