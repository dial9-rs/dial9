// Unit tests for the overview-minimap POI tick derivation. Synthetic worker
// events drive the frozen detectors so the assertions are deterministic (the
// same detector functions the issues rail uses; no code dependency).

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
  cpuTime = 0,
): TraceEvent {
  return {
    eventType,
    workerId,
    timestamp,
    taskId,
    localQueue: 0,
    globalQueue: 0,
    cpuTime,
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
    cpuSamples: [],
    taskInstrumented: new Map<number, boolean>(),
    taskSpawnTimes: new Map<number, number>(),
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

  it("skips the sched/uninstrumented/spawn detectors when their inputs are absent", () => {
    // No sched-wait, no instrumentation map, no spawn times -> long-poll only.
    const events = [
      ev(EVENT_TYPES.PollStart, 0, 1 * MS, 1),
      ev(EVENT_TYPES.PollEnd, 0, 4 * MS, 1),
    ];
    const pois = deriveMinimapPois(fakeTrace(events));
    for (const p of pois) expect(p.type).toBe("long-poll");
  });

  it("ticks a spawn delay at the spawn, once the trace carries spawn times", () => {
    const spawnTs = 1 * MS;
    const events = [
      // Spawns at 1ms, first polled at 3ms: a 2ms delay, past the 100us floor.
      ev(EVENT_TYPES.PollStart, 0, 3 * MS, 1),
      ev(EVENT_TYPES.PollEnd, 0, 3 * MS + 100_000, 1),
    ];
    const pois = deriveMinimapPois(
      fakeTrace(events, { taskSpawnTimes: new Map([[1, spawnTs]]) }),
    );
    const spawnTicks = pois.filter((p) => p.type === "spawn-delay");
    expect(spawnTicks.length).toBe(1);
    // The tick sits at the SPAWN, not the poll - that is where the wait began.
    expect(spawnTicks[0]?.time).toBe(spawnTs);
    expect(spawnTicks[0]?.value).toBe((3 * MS - spawnTs) / 1000);
  });

  it("emits no spawn tick for a delay under the default floor", () => {
    const spawnTs = 3 * MS - 10_000; // 10us before the poll
    const events = [
      ev(EVENT_TYPES.PollStart, 0, 3 * MS, 1),
      ev(EVENT_TYPES.PollEnd, 0, 3 * MS + 100_000, 1),
    ];
    const pois = deriveMinimapPois(
      fakeTrace(events, { taskSpawnTimes: new Map([[1, spawnTs]]) }),
    );
    expect(pois.filter((p) => p.type === "spawn-delay")).toEqual([]);
  });

  // A 10ms awake period that burned 1ms of CPU: ratio 0.1, so 9ms off CPU.
  const descheduledWorker = (parkCpuTime: number): TraceEvent[] => [
    ev(EVENT_TYPES.WorkerUnpark, 0, 0, 0, 0),
    ev(EVENT_TYPES.WorkerPark, 0, 10 * MS, 0, parkCpuTime),
  ];

  it("ticks an off-cpu-active period when worker CPU time is real", () => {
    const pois = deriveMinimapPois(fakeTrace(descheduledWorker(1 * MS)));
    const offCpu = pois.filter((p) => p.type === "off-cpu-active");
    expect(offCpu.map((p) => [p.time, p.worker, p.value])).toEqual([[0, 0, 9 * MS]]);
  });

  // Off Linux every ratio is 0, so ungated this ticks every active period.
  it("omits off-cpu-active entirely when every CPU-time reading is zero", () => {
    const pois = deriveMinimapPois(fakeTrace(descheduledWorker(0)));
    expect(pois.filter((p) => p.type === "off-cpu-active")).toEqual([]);
  });

  // A zero-length period gets ratio 1.0 from the builders' fallback, not from
  // real CPU time, so one of them must not vouch for the whole trace.
  it("is not fooled into enabling off-cpu-active by a zero-length period", () => {
    const events = [
      // Same timestamp: wall 0, ratio falls back to 1.
      ev(EVENT_TYPES.WorkerUnpark, 0, 0, 0, 0),
      ev(EVENT_TYPES.WorkerPark, 0, 0, 0, 0),
      // 10ms, entirely off CPU: cpuTime never advances.
      ev(EVENT_TYPES.WorkerUnpark, 0, 1 * MS, 0, 0),
      ev(EVENT_TYPES.WorkerPark, 0, 11 * MS, 0, 0),
    ];
    const pois = deriveMinimapPois(fakeTrace(events));
    expect(pois.filter((p) => p.type === "off-cpu-active")).toEqual([]);
  });

  // Depends on attachCpuSamples, same reason cpu-sampled is left out.
  it("never ticks off-cpu-poll", () => {
    const events = [
      ev(EVENT_TYPES.PollStart, 0, 1 * MS, 1),
      ev(EVENT_TYPES.PollEnd, 0, 4 * MS, 1),
    ];
    const pois = deriveMinimapPois(fakeTrace(events));
    expect(pois.filter((p) => p.type === "off-cpu-poll")).toEqual([]);
  });
});
