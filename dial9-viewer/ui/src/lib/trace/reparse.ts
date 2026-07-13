// lib/trace/reparse.ts - time-range windowed re-parse (T09; architecture
// 2.7; features/02 E3/E4 Set/Clear Range, B14). Mechanism ported out of
// viewer.html's reparseWithRange (viewer.html:1907-1923): re-parse the
// retained trace buffer with a different time filter, no network fetch.
// URL param updates, loading-view state and the Clear-Range button
// visibility are page concerns.
//
// Segment WINDOWING (the 2.8 lazy segment pipeline) is out of scope here
// (T17); this is only the whole-buffer range filter behind Set/Clear Range.

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
 * are only forwarded when set, matching the legacy behavior (an absent
 * bound is genuinely open, not 0). Extra parse options (maxEvents,
 * onParseProgress, ...) pass through; any startTime/endTime already on
 * `opts` is overridden by `range`.
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
