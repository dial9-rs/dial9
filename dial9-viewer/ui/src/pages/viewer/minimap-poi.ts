// Points-of-interest ticks for the overview minimap. Same detector source as
// the issues rail (filterPointsOfInterest over reconstructed worker spans),
// read independently.
//
// The minimap is an OVERVIEW, so it unions the applicable detectors instead of
// a single selected filter: every notable point earns a tick. Derivation is
// O(events) and belongs in a trace-keyed derived() cache (the component
// installs it), so pan/zoom never re-derive it.
//
// "cpu-sampled" is deliberately excluded: it needs attachCpuSamples, which
// MUTATES the shared poll objects the lanes/overlay caches also hold - the
// minimap must not reach into that shared derivation. "off-cpu-poll" is
// excluded for the same reason (it too keys off the attached samples). The
// remaining detectors (long-poll, sched, wake-delay, uninstrumented,
// spawn-delay, off-cpu-active) read the spans read-only.
//
// "spawn-delay" runs at its DEFAULT threshold, not the rail's live one: these
// ticks are cached on trace identity, and rebuilding them on every threshold
// edit would trade a stable overview for a flickering one.

import {
  DEFAULT_SPAWN_DELAY_THRESHOLD_US,
  EVENT_TYPES,
  filterPointsOfInterest,
} from "../../lib/trace/index.js";
import { lifecycleWorkerIds, sharedDetectorInputs } from "../../lib/trace/derived.js";
import type {
  ParsedTrace,
  PointOfInterest,
  PointOfInterestType,
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

/**
 * Derive the overview POI ticks for one parsed trace: reconstruct worker
 * spans, compute scheduling delays, then union the applicable detectors. Each
 * detector runs with `sortByWorst` on; ticks are de-duplicated by (time,
 * worker, type) since a poll can satisfy more than one detector. Returns []
 * for an empty trace.
 */
export function deriveMinimapPois(trace: ParsedTrace): MinimapPoi[] {
  const workerIds = lifecycleWorkerIds(trace);
  if (workerIds.length === 0) return [];

  // Shared with the issues rail. The minimap's detectors (long-poll, sched,
  // wake-delay, uninstrumented, off-cpu-active) read the spans READ-ONLY, so
  // one shared reconstruction (attachCpuSamples already applied) yields
  // identical ticks.
  const { lanes, schedDelays, hasWorkerCpuTime } = sharedDetectorInputs(trace);

  // Applicable detectors: long-poll always; the sched-derived ones only when
  // the trace carries sched-wait data; uninstrumented only when the trace
  // tracked per-task instrumentation (its required input); off-cpu-active only
  // when the worker CPU-time readings are real.
  const types: PointOfInterestType[] = ["long-poll"];
  if (trace.hasSchedWait) {
    types.push("sched", "wake-delay");
  }
  if (trace.taskInstrumented.size > 0) {
    types.push("uninstrumented");
  }
  // `hasTaskTracking` is NOT the signal - the parser hardcodes it true - so the
  // spawn map's own size is what says whether task tracking was on.
  if (trace.taskSpawnTimes.size > 0) {
    types.push("spawn-delay");
  }
  if (hasWorkerCpuTime) {
    types.push("off-cpu-active");
  }

  const seen = new Set<string>();
  const out: MinimapPoi[] = [];
  for (const type of types) {
    const opts = {
      hasSchedWait: trace.hasSchedWait,
      sortByWorst: true,
      taskInstrumented: trace.taskInstrumented,
      taskSpawnTimes: trace.taskSpawnTimes,
      spawnDelayThresholdUs: DEFAULT_SPAWN_DELAY_THRESHOLD_US,
      hasWorkerCpuTime,
    };
    const pois: PointOfInterest[] = lanes.columnar
      ? lanes.store.pointsOfInterest(type, workerIds, schedDelays, opts)
      : filterPointsOfInterest(type, lanes.workerSpans, workerIds, schedDelays, opts);
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
