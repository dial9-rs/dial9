import type { ParsedTrace } from "../../lib/trace/index.js";

export interface TraceDisplayBounds {
  minTs: number;
  maxTs: number;
}

/** Whole-trace bounds for navigation and display, excluding non-sliceable metadata. */
export function traceDisplayBounds(
  trace: Pick<ParsedTrace, "minTs" | "maxTs" | "recordMinTs" | "recordMaxTs">,
): TraceDisplayBounds | null {
  const hasRecordBounds = trace.recordMinTs != null && trace.recordMaxTs != null;
  const minTs = hasRecordBounds ? trace.recordMinTs : trace.minTs;
  const maxTs = hasRecordBounds ? trace.recordMaxTs : trace.maxTs;
  if (minTs === null || maxTs === null || maxTs < minTs) return null;
  return { minTs, maxTs: maxTs === minTs ? minTs + 1 : maxTs };
}
