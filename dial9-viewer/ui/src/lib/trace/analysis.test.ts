// analysis.ts wiring smoke checks: the CJS-interop named re-exports resolve
// and behave (the core's own behavior stays covered by its Node suites).

import { describe, expect, it } from "vitest";
import {
  buildActiveTaskTimeline,
  buildWorkerSpans,
  getTraceTimeRange,
  hasCpuProfileSamples,
} from "./analysis.js";
import { EVENT_TYPES } from "./load.js";

describe("analysis.ts wiring", () => {
  it("buildWorkerSpans reconstructs a poll from PollStart/PollEnd", () => {
    const base = {
      workerId: 0,
      localQueue: 0,
      globalQueue: 0,
      cpuTime: 0,
      schedWait: 0,
      taskId: 7,
      spawnLocId: null,
      spawnLoc: null,
    };
    const events = [
      { ...base, eventType: EVENT_TYPES.PollStart, timestamp: 100 },
      { ...base, eventType: EVENT_TYPES.PollEnd, timestamp: 200 },
    ];
    const { workerSpans } = buildWorkerSpans(events, [0], 300);
    expect(workerSpans[0]?.polls).toHaveLength(1);
    expect(workerSpans[0]?.polls[0]).toMatchObject({
      start: 100,
      end: 200,
      taskId: 7,
    });
  });

  it("buildWorkerSpans drops a poll left open across an unpark", () => {
    const base = {
      workerId: 0,
      localQueue: 0,
      globalQueue: 0,
      cpuTime: 0,
      schedWait: 0,
      spawnLocId: null,
      spawnLoc: null,
    };
    // Task 7's PollEnd and the park that would have bounded it were both lost.
    // The unpark proves the worker sat parked, so the 30ms up to it is not a
    // poll of task 7 - only task 8's explicitly ended poll survives.
    const events = [
      { ...base, taskId: 7, eventType: EVENT_TYPES.PollStart, timestamp: 1_000 },
      { ...base, taskId: 0, eventType: EVENT_TYPES.WorkerUnpark, timestamp: 31_000 },
      { ...base, taskId: 8, eventType: EVENT_TYPES.PollStart, timestamp: 31_000 },
      { ...base, taskId: 8, eventType: EVENT_TYPES.PollEnd, timestamp: 31_100 },
    ];
    const { workerSpans } = buildWorkerSpans(events, [0], 32_000);
    expect(workerSpans[0]?.polls).toHaveLength(1);
    expect(workerSpans[0]?.polls[0]).toMatchObject({
      start: 31_000,
      end: 31_100,
      taskId: 8,
    });
  });

  it("buildActiveTaskTimeline steps on spawn and terminate", () => {
    const { activeTaskSamples } = buildActiveTaskTimeline(
      new Map([
        [1, 10],
        [2, 20],
      ]),
      new Map([[1, 30]])
    );
    expect(activeTaskSamples.length).toBeGreaterThan(0);
  });

  it("getTraceTimeRange and hasCpuProfileSamples handle empty inputs", () => {
    expect(hasCpuProfileSamples([])).toBe(false);
    expect(getTraceTimeRange([], [], [])).toBeNull();
  });
});
