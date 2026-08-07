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
