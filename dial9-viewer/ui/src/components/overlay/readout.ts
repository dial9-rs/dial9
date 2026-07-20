// The at-cursor info readout, rehomed into the persistent inspector surface.
// Computes the readout and writes it to the `transient.atCursor` store
// contract; the inspector renders it.
//
// PURE compute (Node-testable): the readout is bounded lookups over the
// overlay's derived series (data.ts), never a trace rescan.

import type { AtCursorReadout, SegmentEntry } from "../../types/state.js";
import { activeTaskCountAt, nearestByT } from "../../lib/trace/query.js";

/** The frame-invariant series the readout reads (subset of OverlayData). */
export interface AtCursorInput {
  workerIds: readonly number[];
  queueSamples: readonly { t: number; global: number }[];
  workerQueueSamples: Readonly<Record<number, readonly { t: number; local: number }[]>>;
  activeTaskSamples: readonly { t: number; count: number }[];
}

/**
 * Windowed-data completeness at `ns`. Maps the covering segment's residency
 * onto the readout's coverage signal so the surface never presents a truncated
 * window as whole: an "oversized" covering segment -> "oversized"; a
 * listed/fetching/evicted one (partial data at this edge) -> "truncated"; a
 * parsed one, or the whole-trace (non-segmented) path where the map is empty,
 * -> "complete".
 */
export function coverageAt(
  segments: ReadonlyMap<string, SegmentEntry>,
  ns: number,
): AtCursorReadout["coverage"] {
  if (segments.size === 0) return "complete";
  let covering: SegmentEntry | null = null;
  for (const seg of segments.values()) {
    if (ns >= seg.extent.startNs && ns <= seg.extent.endNs) {
      covering = seg;
      break;
    }
  }
  if (covering === null) return "complete";
  switch (covering.state) {
    case "oversized":
      return "oversized";
    case "parsed":
      return "complete";
    case "listed":
    case "fetching":
    case "evicted":
      return "truncated";
  }
}

/**
 * Compute the at-cursor readout at `ns` for the worker under the cursor
 * (`workerId`, null when off any lane): nearest global-queue sample, MAX
 * local-queue across workers, step active-task count. `activeTaskCount` is null
 * when the trace has no task timeline (the line is omitted then).
 */
export function computeAtCursorReadout(
  input: AtCursorInput,
  ns: number,
  workerId: number | null,
  coverage: AtCursorReadout["coverage"],
): AtCursorReadout {
  const nearestGlobal = nearestByT(input.queueSamples, ns);

  let localMax: number | null = null;
  for (const w of input.workerIds) {
    const samples = input.workerQueueSamples[w];
    if (!samples || samples.length === 0) continue;
    const s = nearestByT(samples, ns);
    if (s !== null) localMax = localMax === null ? s.local : Math.max(localMax, s.local);
  }

  return {
    ns,
    workerId,
    globalQueue: nearestGlobal !== null ? nearestGlobal.global : null,
    localMax,
    activeTaskCount: activeTaskCountAt(input.activeTaskSamples, ns),
    coverage,
  };
}
