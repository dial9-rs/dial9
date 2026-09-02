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
  // EMPTY_QUEUE_DATA is the no-trace default, where queue depth is
  // unavailable. These fixtures stand in for a real trace, so opt it back on.
  return { ...EMPTY_QUEUE_DATA, hasLocalQueueDepth: true, ...over };
}

describe("queueScaleY (#282: 0 must render a visible baseline)", () => {
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

describe("buildQueueRenderModel", () => {
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

  it("tracks active-task start count, in-view peak, and points", () => {
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

  it("bins task spawns at viewport-adaptive fixed-pixel resolution", () => {
    const m = buildQueueRenderModel({
      data: queueData({
        taskSpawnTimes: [0, 1, 39, 40, 80, 120, 121],
      }),
      viewStart: 0,
      viewEnd: 120,
      drawW: 24,
    });
    expect(m.spawnHistogram).toEqual({
      counts: [3, 1, 2],
      maxSpawns: 3,
      binWidthPx: 8,
      binDurationNs: 40,
    });
    expect(m.hasData).toBe(true);
  });

  it("omits the spawn histogram when no spawn falls inside the viewport", () => {
    const m = buildQueueRenderModel({
      data: queueData({ taskSpawnTimes: [-10, 110] }),
      viewStart: 0,
      viewEnd: 100,
      drawW: 24,
    });
    expect(m.spawnHistogram).toBeNull();
  });

  it("uses one spawn bucket for canvases narrower than the target bin width", () => {
    const m = buildQueueRenderModel({
      data: queueData({ taskSpawnTimes: [1, 2, 3] }),
      viewStart: 0,
      viewEnd: 10,
      drawW: 4,
    });
    expect(m.spawnHistogram).toEqual({
      counts: [3],
      maxSpawns: 3,
      binWidthPx: 4,
      binDurationNs: 10,
    });
  });

  it("carries the first sampled task count backward instead of inventing zero", () => {
    const m = buildQueueRenderModel({
      data: queueData({
        activeTaskSamples: [
          { t: 25, count: 56 },
          { t: 75, count: 50 },
        ],
      }),
      viewStart: 0,
      viewEnd: 100,
      drawW: 100,
    });
    expect(m.activeTask!.startCount).toBe(56);
    expect(m.activeTask!.maxTasks).toBe(56);
    expect(m.activeTask!.points[0]).toEqual({ x: 25, count: 56 });
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

  it("active-task window is bounded when panned far right (many samples before view)", () => {
    // Dense samples before the view; the binary-searched window must still seed
    // startCount from the last pre-view sample and only emit in-view points.
    const before = Array.from({ length: 1000 }, (_, i) => ({ t: i, count: i % 5 }));
    const inView = [{ t: 5000, count: 42 }, { t: 5050, count: 7 }];
    const m = buildQueueRenderModel({
      data: queueData({ activeTaskSamples: [...before, ...inView, { t: 9999, count: 100 }] }),
      viewStart: 4900,
      viewEnd: 5100,
      drawW: 200,
    });
    expect(m.activeTask!.startCount).toBe(999 % 5); // last sample with t <= viewStart
    expect(m.activeTask!.maxTasks).toBe(42); // peak in-view, not the 100 past it
    expect(m.activeTask!.points).toHaveLength(2);
    expect(m.activeTask!.points[0]!.count).toBe(42);
  });
});

// A sentinel 0 plotted as a real series draws a flat zero line, which reads as
// "the local queues are empty" rather than "unmeasured".
describe("local series when queue depth is unavailable", () => {
  const samples = [
    { t: 0, w: 0, local: 0 },
    { t: 50, w: 0, local: 0 },
  ];

  it("drops the series when hasLocalQueueDepth is false", () => {
    const m = buildQueueRenderModel({
      data: queueData({
        hasLocalQueueDepth: false,
        workerIds: [0],
        mergedLocalSamples: samples,
      }),
      viewStart: 0,
      viewEnd: 100,
      drawW: 10,
    });
    expect(m.local).toEqual([]);
  });

  it("sentinel samples never mark buckets or reset the global carry", () => {
    // One global sample at t=5, sentinel locals at t=0/50. 10 buckets over
    // [0,100]: the global value 3 carries to the last bucket; the t=50
    // sentinel must not zero it from bucket 5 on.
    const m = buildQueueRenderModel({
      data: queueData({
        hasLocalQueueDepth: false,
        workerIds: [0],
        queueSamples: [{ t: 5, global: 3 }],
        mergedLocalSamples: samples,
      }),
      viewStart: 0,
      viewEnd: 100,
      drawW: 10,
    });
    expect(m.global[m.global.length - 1]).toBe(3);
    expect(m.hasData).toBe(true);

    // Sentinels alone are not data.
    const empty = buildQueueRenderModel({
      data: queueData({
        hasLocalQueueDepth: false,
        workerIds: [0],
        mergedLocalSamples: samples,
      }),
      viewStart: 0,
      viewEnd: 100,
      drawW: 10,
    });
    expect(empty.hasData).toBe(false);
  });

  it("keeps a genuine zero series when the depth is real", () => {
    const m = buildQueueRenderModel({
      data: queueData({
        hasLocalQueueDepth: true,
        workerIds: [0],
        mergedLocalSamples: samples,
      }),
      viewStart: 0,
      viewEnd: 100,
      drawW: 10,
    });
    expect(m.local.length).toBeGreaterThan(0);
    expect(m.local.every((v) => v === 0)).toBe(true);
  });
});

describe("computeSpawnedTasks", () => {
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

  it("includes both range bounds", () => {
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

  it("uses sampled active-task totals instead of restarting partial traces at zero", () => {
    const trace = {
      events: [],
      maxTs: 1000,
      blockInPlaceGaps: [],
      runtimeWorkers: new Map(),
      runtimeMetrics: [
        { t: 100, runtimeName: "", globalQueue: 0, aliveTasks: 40 },
        { t: 100, runtimeName: "io", globalQueue: 0, aliveTasks: 3 },
      ],
      legacyActiveTaskSamples: [],
      taskSpawnTimes: new Map([[1, 110]]),
      taskTerminateTimes: new Map(),
      taskSpawnLocs: new Map([[1, "spawn.rs:1"]]),
      spawnLocations: new Map([["spawn.rs:1", "spawn.rs:1"]]),
      hasTaskTracking: true,
      hasLocalQueueDepth: false,
    } as unknown as ParsedTrace;

    const data = computeQueueData(trace);
    expect(data.activeTaskSamples).toEqual([{ t: 100, count: 43 }]);
    expect(data.taskSpawnTimes).toEqual([110]);
    expect(data.taskFirstPoll.get(1)).toBe(110);
  });

  it("sorts task spawn timestamps for histogram windowing", () => {
    const trace = {
      events: [],
      maxTs: 1000,
      blockInPlaceGaps: [],
      runtimeWorkers: new Map(),
      runtimeMetrics: [],
      legacyActiveTaskSamples: [],
      taskSpawnTimes: new Map([
        [1, 300],
        [2, 100],
        [3, 200],
      ]),
      taskTerminateTimes: new Map(),
      taskSpawnLocs: new Map(),
      spawnLocations: new Map(),
      hasTaskTracking: true,
      hasLocalQueueDepth: false,
    } as unknown as ParsedTrace;

    expect(computeQueueData(trace).taskSpawnTimes).toEqual([100, 200, 300]);
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

// ── Equivalence with the pre-rewrite bucketing ───────────────────────────────
//
// buildQueueRenderModel used to be O(pixels x workers): every pixel column
// re-scanned every worker's depth to find the max. It is now an incremental
// running max, O(pixels + transitions). That is a pure performance change, so
// the output must be bit-identical - this pins it against a transcription of
// the original algorithm over randomized fixtures.

/** The original local-queue bucketing, transcribed verbatim. */
function referenceLocal(
  data: QueueData,
  viewStart: number,
  viewEnd: number,
  numBuckets: number,
): { local: number[]; hasData: boolean[] } {
  const viewDur = viewEnd - viewStart;
  const merged = data.mergedLocalSamples;
  const workerState = new Map<number, number>();
  for (const w of data.workerIds) workerState.set(w, 0);

  const lower = (arr: readonly { t: number }[], target: number): number => {
    let lo = 0, hi = arr.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (arr[mid]!.t < target) lo = mid + 1;
      else hi = mid;
    }
    return lo;
  };

  const mergeStart = Math.max(0, lower(merged, viewStart) - 1);
  for (let i = mergeStart; i < merged.length; i++) {
    const s = merged[i]!;
    if (s.t >= viewStart) break;
    workerState.set(s.w, s.local);
  }
  const bucketLocal = new Array<number>(numBuckets).fill(0);
  const bucketHasData = new Array<boolean>(numBuckets).fill(false);
  let sampleIdx = Math.max(mergeStart, lower(merged, viewStart));
  for (let bi = 0; bi < numBuckets; bi++) {
    const bucketEnd = viewStart + ((bi + 1) / numBuckets) * viewDur;
    while (sampleIdx < merged.length && merged[sampleIdx]!.t < bucketEnd) {
      const s = merged[sampleIdx++]!;
      workerState.set(s.w, s.local);
      bucketHasData[bi] = true;
    }
    let max = 0;
    for (const w of data.workerIds) {
      const v = workerState.get(w) ?? 0;
      if (v > max) max = v;
    }
    bucketLocal[bi] = max;
  }
  return { local: bucketLocal, hasData: bucketHasData };
}

/** Deterministic PRNG so a failure is reproducible from its seed. */
function rng(seed: number): () => number {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 0x100000000;
  };
}

function randomQueueData(seed: number, workerCount: number, samples: number): QueueData {
  const r = rng(seed);
  const workerIds = Array.from({ length: workerCount }, (_, i) => i);
  const merged: { t: number; w: number; local: number }[] = [];
  for (let i = 0; i < samples; i++) {
    merged.push({
      t: Math.floor(r() * 10_000),
      w: Math.floor(r() * workerCount),
      // Depths that repeatedly revisit the peak, so the max walks down often.
      local: Math.floor(r() * 6),
    });
  }
  merged.sort((a, b) => a.t - b.t);
  const queueSamples = Array.from({ length: samples }, () => ({
    t: Math.floor(r() * 10_000),
    global: Math.floor(r() * 8),
  })).sort((a, b) => a.t - b.t);

  return {
    ...EMPTY_QUEUE_DATA,
    hasLocalQueueDepth: true,
    workerIds,
    mergedLocalSamples: merged,
    queueSamples,
  };
}

describe("buildQueueRenderModel equivalence", () => {
  it("matches the reference local bucketing across random fixtures", () => {
    for (let seed = 1; seed <= 25; seed++) {
      const workerCount = 1 + (seed % 9);
      const data = randomQueueData(seed, workerCount, 200);
      const drawW = 40 + (seed % 30);
      const viewStart = 0;
      const viewEnd = 10_000;
      const numBuckets = Math.max(0, Math.ceil(drawW));

      const got = buildQueueRenderModel({ data, viewStart, viewEnd, drawW });
      const want = referenceLocal(data, viewStart, viewEnd, numBuckets);

      // Carry-forward is applied on top of the raw buckets, so compare the
      // plotted series the reference implies rather than the raw buckets.
      let lastL = 0;
      const wantPlotted = want.local.map((v, i) => {
        if (want.hasData[i]) lastL = v;
        return lastL;
      });
      expect([...got.local], `seed=${seed}`).toEqual(wantPlotted);
    }
  });

  it("ignores samples for workers outside the id set, as the original did", () => {
    // A merged sample for worker 99 must not raise the plotted max: the old
    // implementation took its max by iterating workerIds.
    const data: QueueData = {
      ...EMPTY_QUEUE_DATA,
      hasLocalQueueDepth: true,
      workerIds: [0, 1],
      mergedLocalSamples: [
        { t: 10, w: 0, local: 2 },
        { t: 20, w: 99, local: 500 },
      ],
    };
    const model = buildQueueRenderModel({ data, viewStart: 0, viewEnd: 100, drawW: 10 });
    expect(Math.max(...model.local)).toBe(2);
  });

  it("does not leak state between successive frames", () => {
    // The bucket scratch and the running max are reused across calls; a stale
    // read would show up as one frame's peak bleeding into the next.
    const busy: QueueData = {
      ...EMPTY_QUEUE_DATA,
      hasLocalQueueDepth: true,
      workerIds: [0],
      mergedLocalSamples: [{ t: 10, w: 0, local: 9 }],
    };
    const idle: QueueData = {
      ...EMPTY_QUEUE_DATA,
      hasLocalQueueDepth: true,
      workerIds: [0],
      mergedLocalSamples: [],
    };
    buildQueueRenderModel({ data: busy, viewStart: 0, viewEnd: 100, drawW: 20 });
    const second = buildQueueRenderModel({ data: idle, viewStart: 0, viewEnd: 100, drawW: 20 });
    expect(Math.max(...second.local)).toBe(0);
    expect(second.maxQ).toBe(1);
  });
});
