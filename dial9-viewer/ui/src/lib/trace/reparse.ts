// Time-range windowed re-parse: re-parse the retained trace buffer with a
// different time filter, no network fetch (Set/Clear Range). URL param
// updates, loading-view state and Clear-Range visibility are page concerns.
// Whole-buffer only; the lazy per-segment pipeline lives in ./segments.ts.

import type { ParseOptions, ParsedTrace } from "../../../trace_parser.js";
import { parseTraceBuffer } from "./load.js";

/**
 * The Set/Clear Range window, in absolute trace nanoseconds. `null` /
 * absent bounds are open (Clear Range passes both as null).
 */
export interface ReparseRange {
  startNs?: number | null;
  endNs?: number | null;
}

/** True when the range constrains at least one edge (drives Clear-Range visibility). */
export function isRangeActive(range: ReparseRange): boolean {
  return range.startNs != null || range.endNs != null;
}

/**
 * Re-parse an already-loaded trace buffer with a new time filter. Bounds
 * are only forwarded when set (an absent bound is genuinely open, not 0).
 * Extra parse options (maxEvents, onParseProgress, ...) pass through; any
 * startTime/endTime already on `opts` is overridden by `range`.
 */
export function reparseWithRange(
  buffer: ArrayBuffer | Uint8Array,
  range: ReparseRange,
  opts: ParseOptions = {}
): Promise<ParsedTrace> {
  const parseOpts: ParseOptions = { ...opts };
  delete parseOpts.startTime;
  delete parseOpts.endTime;
  if (range.startNs != null) parseOpts.startTime = range.startNs;
  if (range.endNs != null) parseOpts.endTime = range.endNs;
  return parseTraceBuffer(buffer, parseOpts);
}
