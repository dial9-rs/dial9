// src/pages/viewer/minimap-poi.ts - points-of-interest ticks for the overview
// minimap (T35). Same detector SOURCE as T33's issues rail
// (lib/trace/analysis filterPointsOfInterest over reconstructed worker spans)
// with NO code dependency between the two tickets - both read the frozen
// core's detector output independently (chunk-2 ticket boundary).
//
// The minimap is an OVERVIEW, so it unions the applicable detectors instead of
// a single selected filter: every notable point earns a tick, giving the
// position context 04 S8 asks for. Derivation is O(events) (one worker-span
// reconstruction + one sched-delay pass) and belongs in a trace-keyed
// derived() cache (the component installs it), so pan/zoom never re-derive it.
//
// "cpu-sampled" is deliberately excluded: it needs attachCpuSamples, which
// MUTATES the shared poll objects the lanes/overlay caches also hold - the
// minimap must not reach into that shared derivation. The remaining detectors
// (long-poll, sched, wake-delay, uninstrumented) read the spans read-only.

import {
  EVENT_TYPES,
  buildWorkerSpans,
  computeSchedulingDelays,
  filterPointsOfInterest,
} from "../../lib/trace/index.js";
import type {
  ParsedTrace,
  PointOfInterest,
  PointOfInterestType,
  WorkerLane,
} from "../../lib/trace/index.js";

/** One overview tick: where a point of interest sits, and what kind. */
export interface MinimapPoi {
  /** Timestamp (trace-monotonic ns). */
  time: number;
  type: PointOfInterestType;
  worker: number;
  /** Detector value (delay/duration ns); for tooltip/sorting parity. */
  value: number;
}

/** Distinct worker ids that appear in lifecycle events, sorted. */
function lifecycleWorkerIds(trace: ParsedTrace): number[] {
  const set = new Set<number>();
  for (const e of trace.events) {
    if (
      e.eventType === EVENT_TYPES.QueueSample ||
      e.eventType === EVENT_TYPES.WakeEvent
    ) {
      continue;
    }
    set.add(e.workerId);
  }
  return [...set].sort((a, b) => a - b);
}

/**
 * Derive the overview POI ticks for one parsed trace: reconstruct worker
 * spans, compute scheduling delays, then union the applicable detectors. Each
 * detector runs with `sortByWorst` on (legacy default), matching the rail's
 * ranking; ticks are de-duplicated by (time, worker, type) since a poll can
 * legitimately satisfy more than one detector. Returns [] for an empty trace.
 */
export function deriveMinimapPois(trace: ParsedTrace): MinimapPoi[] {
  const workerIds = lifecycleWorkerIds(trace);
  if (workerIds.length === 0) return [];
  const maxTs = trace.maxTs ?? 0;

  const spanResult = buildWorkerSpans(
    trace.events,
    workerIds,
    maxTs,
    trace.blockInPlaceGaps,
  );
  const workerSpans: Record<number, WorkerLane> = spanResult.workerSpans;
  const schedDelays = computeSchedulingDelays(
    workerSpans,
    workerIds,
    spanResult.wakesByTask,
  );

  // Applicable detectors: long-poll always; the sched-derived ones only when
  // the trace carries sched-wait data; uninstrumented only when the trace
  // tracked per-task instrumentation (its required input).
  const types: PointOfInterestType[] = ["long-poll"];
  if (trace.hasSchedWait) {
    types.push("sched", "wake-delay");
  }
  if (trace.taskInstrumented.size > 0) {
    types.push("uninstrumented");
  }

  const seen = new Set<string>();
  const out: MinimapPoi[] = [];
  for (const type of types) {
    const pois: PointOfInterest[] = filterPointsOfInterest(
      type,
      workerSpans,
      workerIds,
      schedDelays,
      {
        hasSchedWait: trace.hasSchedWait,
        sortByWorst: true,
        taskInstrumented: trace.taskInstrumented,
      },
    );
    for (const p of pois) {
      const dedupeKey = `${p.time}:${p.worker}:${p.type}`;
      if (seen.has(dedupeKey)) continue;
      seen.add(dedupeKey);
      out.push({ time: p.time, type: p.type, worker: p.worker, value: p.value });
    }
  }
  out.sort((a, b) => a.time - b.time);
  return out;
}
