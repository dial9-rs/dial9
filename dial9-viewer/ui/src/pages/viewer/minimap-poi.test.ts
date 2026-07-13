// Unit tests for the overview-minimap POI tick derivation (T35). Synthetic
// worker events drive the frozen detectors so the assertions are deterministic
// (the same detector functions T33's rail uses; no code dependency).

import { describe, it, expect } from "vitest";
import { EVENT_TYPES } from "../../lib/trace/index.js";
import type { ParsedTrace, TraceEvent } from "../../lib/trace/index.js";
import { deriveMinimapPois } from "./minimap-poi.js";

const MS = 1_000_000; // 1ms in ns; long-poll detector floor is 1ms.

function ev(
  eventType: number,
  workerId: number,
  timestamp: number,
  taskId: number,
): TraceEvent {
  return {
    eventType,
    workerId,
    timestamp,
    taskId,
    localQueue: 0,
    globalQueue: 0,
    cpuTime: 0,
    schedWait: 0,
    spawnLocId: null,
    spawnLoc: null,
  } as unknown as TraceEvent;
}

/** A ParsedTrace stub carrying only what deriveMinimapPois reads. */
function fakeTrace(events: TraceEvent[], overrides: Partial<ParsedTrace> = {}): ParsedTrace {
  const maxTs = events.reduce((m, e) => Math.max(m, e.timestamp), 0);
  return {
    events,
    maxTs,
    blockInPlaceGaps: [],
    hasSchedWait: false,
    taskInstrumented: new Map<number, boolean>(),
    ...overrides,
  } as unknown as ParsedTrace;
}

describe("deriveMinimapPois", () => {
  it("returns [] for a trace with no lifecycle events", () => {
    expect(deriveMinimapPois(fakeTrace([]))).toEqual([]);
  });

  it("finds long-poll ticks (>1ms polls) and sorts them by time", () => {
    const events = [
      // worker 1: a 5ms poll (long) at 10ms
      ev(EVENT_TYPES.PollStart, 1, 10 * MS, 100),
      ev(EVENT_TYPES.PollEnd, 1, 15 * MS, 100),
      // worker 0: a 3ms poll (long) at 2ms
      ev(EVENT_TYPES.PollStart, 0, 2 * MS, 200),
      ev(EVENT_TYPES.PollEnd, 0, 5 * MS, 200),
      // worker 0: a 0.2ms poll (NOT long) at 6ms
      ev(EVENT_TYPES.PollStart, 0, 6 * MS, 200),
      ev(EVENT_TYPES.PollEnd, 0, 6 * MS + 200_000, 200),
    ];
    const pois = deriveMinimapPois(fakeTrace(events));
    expect(pois.length).toBeGreaterThanOrEqual(2);
    // Every tick carries the detector's value and they are time-ordered.
    for (const p of pois) expect(p.value).toBeGreaterThan(0);
    const times = pois.map((p) => p.time);
    expect([...times]).toEqual([...times].sort((a, b) => a - b));
    // The two long polls' start times are present.
    expect(times).toContain(2 * MS);
    expect(times).toContain(10 * MS);
    // The sub-ms poll at 6ms is not a long-poll tick.
    expect(pois.filter((p) => p.type === "long-poll").map((p) => p.time)).not.toContain(
      6 * MS,
    );
  });

  it("de-duplicates ticks that satisfy the same detector once", () => {
    const events = [
      ev(EVENT_TYPES.PollStart, 0, 1 * MS, 1),
      ev(EVENT_TYPES.PollEnd, 0, 4 * MS, 1),
    ];
    const pois = deriveMinimapPois(fakeTrace(events));
    const keys = pois.map((p) => `${p.time}:${p.worker}:${p.type}`);
    expect(new Set(keys).size).toBe(keys.length);
  });

  it("skips the sched/uninstrumented detectors when their inputs are absent", () => {
    // hasSchedWait=false and empty taskInstrumented -> only long-poll runs, so
    // no "sched"/"wake-delay"/"uninstrumented" ticks appear.
    const events = [
      ev(EVENT_TYPES.PollStart, 0, 1 * MS, 1),
      ev(EVENT_TYPES.PollEnd, 0, 4 * MS, 1),
    ];
    const pois = deriveMinimapPois(fakeTrace(events));
    for (const p of pois) expect(p.type).toBe("long-poll");
  });
});
