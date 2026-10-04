import type { ParsedTrace } from "../../lib/trace/index.js";

export interface TraceDisplayBounds {
  minTs: number;
  maxTs: number;
}

/** Whole-trace bounds for navigation and display, excluding metadata and task dumps. */
export function traceDisplayBounds(
  trace: Pick<
    ParsedTrace,
    | "minTs"
    | "maxTs"
    | "recordMinTs"
    | "recordMaxTs"
    | "displayMinTs"
    | "displayMaxTs"
  >,
): TraceDisplayBounds | null {
  const hasDisplayBounds =
    trace.displayMinTs !== undefined && trace.displayMaxTs !== undefined;
  const hasRecordBounds = trace.recordMinTs != null && trace.recordMaxTs != null;
  const minTs = hasDisplayBounds
    ? trace.displayMinTs
    : hasRecordBounds
      ? trace.recordMinTs
      : trace.minTs;
  const maxTs = hasDisplayBounds
    ? trace.displayMaxTs
    : hasRecordBounds
      ? trace.recordMaxTs
      : trace.maxTs;
  if (minTs == null || maxTs == null || maxTs < minTs) return null;
  return { minTs, maxTs: maxTs === minTs ? minTs + 1 : maxTs };
}
