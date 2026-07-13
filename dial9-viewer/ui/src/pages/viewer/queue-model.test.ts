// Tests for the Queue depth track's pure model:
//   1. `queueScaleY` maps a 0-valued series to a VISIBLE baseline STRICTLY
//      above the axis (the zero-global-invisible fix, issue #282), and the
//      shared scale is monotone + clamped.
//   2. `buildQueueRenderModel` reproduces the bucketed numbers (maxQ,
//      carry-forward step values, active-task maxTasks/startCount) and
//      `computeSpawnedTasks` reproduces the task-finding + grouping.
//      Presentation changed; the numbers did not.

import { describe, it, expect } from "vitest";
import {
  ZERO_BASELINE_PX,
  buildQueueRenderModel,
  computeQueueData,
  computeSpawnedTasks,
  deriveQueueWindow,
  queueBaselineY,
  queueScaleY,
  EMPTY_QUEUE_DATA,
  type QueueData,
} from "./queue-model.js";
import type { StoreState } from "../../types/state.js";
import type { ParsedTrace } from "../../types/trace.js";

// A QueueData with only the fields a test needs; the model reads plain arrays.
function queueData(over: Partial<QueueData> = {}): QueueData {
  return { ...EMPTY_QUEUE_DATA, ...over };
}

describe("queueScaleY (S6 / #282: 0 must render a visible baseline)", () => {
  const top = 10;
  const h = 100;

  it("maps value 0 STRICTLY above the axis bottom (not fused to it)", () => {
    const y0 = queueScaleY(0, 5, top, h);
    // The bug: 0 -> chartTop + chartH (the axis) -> invisible. The fix
    // reserves ZERO_BASELINE_PX so 0 lands above the axis.
    expect(y0).toBeLessThan(top + h);
    expect(y0).toBe(top + h - ZERO_BASELINE_PX);
  });

  it("maps the max value to the chart top", () => {
    expect(queueScaleY(5, 5, top, h)).toBe(top);
  });

  it("is monotone: a larger value maps to a smaller y (higher on screen)", () => {
    const yLow = queueScaleY(1, 10, top, h);
    const yHigh = queueScaleY(9, 10, top, h);
    expect(yHigh).toBeLessThan(yLow);
    expect(yLow).toBeLessThan(queueScaleY(0, 10, top, h));
  });

  it("pins everything to the baseline when max <= 0", () => {
    expect(queueScaleY(0, 0, top, h)).toBe(top + h - ZERO_BASELINE_PX);
    expect(queueScaleY(3, 0, top, h)).toBe(top + h - ZERO_BASELINE_PX);
  });

  it("clamps out-of-range values into the band", () => {
    expect(queueScaleY(-2, 5, top, h)).toBe(top + h - ZERO_BASELINE_PX);
    expect(queueScaleY(99, 5, top, h)).toBe(top);
  });

  it("queueBaselineY equals queueScaleY(0)", () => {
    expect(queueBaselineY(top, h)).toBe(queueScaleY(0, 7, top, h));
  });
});

describe("buildQueueRenderModel (J7: legacy bucket numbers preserved)", () => {
  it("returns no-data on an empty view or degenerate window", () => {
    const m = buildQueueRenderModel({
      data: queueData(),
      viewStart: 0,
      viewEnd: 100,
      drawW: 50,
    });
    expect(m.hasData).toBe(false);
    // Degenerate window (viewEnd <= viewStart): no buckets plotted.
    const deg = buildQueueRenderModel({
      data: queueData({ queueSamples: [{ t: 5, global: 3 }] }),
      viewStart: 100,
      viewEnd: 100,
      drawW: 50,
    });
    expect(deg.hasData).toBe(false);
  });

  it("takes the peak global sample per bucket and carries it forward", () => {
    // drawW 4 -> 4 buckets over [0,100]. Global samples at t=10 (=>2) and
    // t=60 (=>5). maxQ = max(1, 5, 0 local) = 5. Values carry forward.
    const m = buildQueueRenderModel({
      data: queueData({
        queueSamples: [
          { t: 10, global: 2 },
          { t: 60, global: 5 },
        ],
      }),
      viewStart: 0,
      viewEnd: 100,
      drawW: 4,
    });
    expect(m.maxQ).toBe(5);
    expect(m.hasData).toBe(true);
    // First non-zero bucket holds 2 and carries until the 5 lands, then 5.
    expect(Math.max(...m.global)).toBe(5);
    expect(m.global[m.global.length - 1]).toBe(5);
    // No local series -> all zero.
    expect(Math.max(...m.local)).toBe(0);
  });

  it("tracks max local across workers per bucket (merged timeline)", () => {
    // Two workers; worker 1 rises to 4, worker 2 to 7 -> max-local peak 7.
    const m = buildQueueRenderModel({
      data: queueData({
        workerIds: [1, 2],
        mergedLocalSamples: [
          { t: 10, w: 1, local: 4 },
          { t: 20, w: 2, local: 7 },
          { t: 80, w: 2, local: 1 },
        ],
      }),
      viewStart: 0,
      viewEnd: 100,
      drawW: 10,
    });
    expect(m.maxQ).toBe(7);
    expect(Math.max(...m.local)).toBe(7);
  });

  it("reproduces legacy active-task maxTasks / startCount / points", () => {
    const m = buildQueueRenderModel({
      data: queueData({
        activeTaskSamples: [
          { t: -10, count: 3 }, // before view -> seeds startCount
          { t: 25, count: 8 },
          { t: 75, count: 2 },
          { t: 250, count: 99 }, // past view -> ignored for maxTasks + points
        ],
      }),
      viewStart: 0,
      viewEnd: 100,
      drawW: 100,
    });
    expect(m.activeTask).not.toBeNull();
    expect(m.activeTask!.startCount).toBe(3);
    expect(m.activeTask!.maxTasks).toBe(8); // peak in-view, not the 99 past it
    expect(m.activeTask!.points).toHaveLength(2);
    expect(m.activeTask!.points[0]).toEqual({ x: 25, count: 8 });
  });

  it("has no active-task overlay when the trace lacks the timeline", () => {
    const m = buildQueueRenderModel({
      data: queueData({ queueSamples: [{ t: 5, global: 1 }] }),
      viewStart: 0,
      viewEnd: 100,
      drawW: 10,
    });
    expect(m.activeTask).toBeNull();
  });
});

describe("computeSpawnedTasks (M7 / J7: legacy task-finding + grouping)", () => {
  const data = queueData({
    hasTaskTracking: true,
    taskFirstPoll: new Map([
      [0x1, 100],
      [0x2, 150],
      [0x3, 900], // out of range
      [0x4, 120],
      [0x5, 130],
    ]),
    taskSpawnLocs: new Map<number, string | null>([
      [0x1, "locA"],
      [0x2, "locB"],
      [0x3, "locA"],
      [0x4, "locA"],
      [0x5, null], // unknown location
    ]),
    spawnLocations: new Map([
      ["locA", "src/a.rs:1"],
      ["locB", "src/b.rs:2"],
    ]),
  });

  it("finds tasks first-polled in range, grouped by loc, sorted by count desc", () => {
    const res = computeSpawnedTasks(data, { startNs: 100, endNs: 200 });
    expect(res).not.toBeNull();
    expect(res!.total).toBe(4); // 0x1,0x2,0x4,0x5 (0x3 is at 900)
    // Group order: src/a.rs:1 (2 tasks) first, then the two 1-task groups.
    expect(res!.groups[0]!.loc).toBe("src/a.rs:1");
    expect(res!.groups[0]!.tasks.map((t) => t.taskId)).toEqual([0x1, 0x4]);
    const locs = res!.groups.map((g) => g.loc);
    expect(locs).toContain("src/b.rs:2");
    expect(locs).toContain("(unknown)"); // task 0x5 has a null spawn loc
  });

  it("bounds are inclusive on both ends (legacy >= / <=)", () => {
    const res = computeSpawnedTasks(data, { startNs: 100, endNs: 100 });
    expect(res!.total).toBe(1);
    expect(res!.groups[0]!.tasks[0]!.taskId).toBe(0x1);
  });

  it("returns null when no task was first-polled in range", () => {
    expect(computeSpawnedTasks(data, { startNs: 500, endNs: 800 })).toBeNull();
  });
});

describe("computeQueueData / deriveQueueWindow", () => {
  it("returns empty data for a null trace", () => {
    expect(computeQueueData(null)).toBe(EMPTY_QUEUE_DATA);
  });

  it("does not throw on a trace with no events (empty series)", () => {
    const trace = {
      events: [],
      maxTs: 1000,
      blockInPlaceGaps: [],
      runtimeWorkers: new Map(),
      taskSpawnTimes: new Map(),
      taskTerminateTimes: new Map(),
      taskSpawnLocs: new Map(),
      spawnLocations: new Map(),
      hasTaskTracking: false,
    } as unknown as ParsedTrace;
    const data = computeQueueData(trace);
    expect(data.workerIds).toEqual([]);
    expect(data.queueSamples).toEqual([]);
    expect(data.hasTaskTracking).toBe(false);
  });

  it("resolves complete for an empty segments slice, oversized when a segment is", () => {
    const complete = deriveQueueWindow({
      segments: { segments: new Map() },
    } as unknown as StoreState);
    expect(complete).toEqual({ truncatedAt: null, oversized: false });

    const oversized = deriveQueueWindow({
      segments: {
        segments: new Map([
          ["seg", { state: "oversized", extent: { startNs: 0, endNs: 1 }, sizeBytes: 1 }],
        ]),
      },
    } as unknown as StoreState);
    expect(oversized.oversized).toBe(true);
  });
});
