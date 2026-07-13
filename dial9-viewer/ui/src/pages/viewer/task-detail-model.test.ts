// Tests for the task-detail track's pure model - the behavioral checks that do
// NOT need a browser: the selection-keyed collection numbers, the waker-label
// resolution, and the per-frame render geometry (delay bands, lifespan, poll
// bars / coverage, idle gaps) with its status + waker hit regions. Poll/wake
// records are built synthetically; one case drives computePollWakes through
// computeTaskDetailData.

import { describe, it, expect } from "vitest";
import { formatHumanDuration } from "../../lib/trace/index.js";
import type {
  ParsedTrace,
  PollSpan,
  TaskDump,
  TaskWake,
} from "../../lib/trace/index.js";
import {
  buildTaskDetailRenderModel,
  computeTaskDetailData,
  EMPTY_TASK_DETAIL_DATA,
  firstVisibleByEnd,
  formatTaskDetailSummary,
  hitRegionAt,
  statusTextAt,
  wakeRegionAt,
  wakerLabelFor,
  BAND_TOP,
  BAND_H,
  type PollWakeInfo,
  type TaskDetailData,
  type TaskPollSource,
} from "./task-detail-model.js";

// ── Synthetic builders ───────────────────────────────────────────────────

function poll(start: number, end: number, taskId: number, spawnLocId = "loc"): PollSpan {
  return { start, end, taskId, spawnLocId, spawnLoc: null };
}

function wake(timestamp: number, wakerTaskId: number, targetWorker = 0): TaskWake {
  return { timestamp, wakerTaskId, targetWorker };
}

function source(o: {
  workerIds: number[];
  pollsByWorker: Record<number, PollSpan[]>;
  wakesByTask?: Record<number, TaskWake[]>;
}): TaskPollSource {
  const workerSpans: TaskPollSource["workerSpans"] = {};
  for (const [w, polls] of Object.entries(o.pollsByWorker)) {
    workerSpans[Number(w)] = {
      polls,
      parks: [],
      actives: [],
      cpuSampleTimes: [],
    };
  }
  return {
    workerIds: o.workerIds,
    workerSpans,
    wakesByTask: o.wakesByTask ?? {},
  };
}

/** A ParsedTrace stub carrying only the task maps computeTaskDetailData reads. */
function fakeTrace(o: {
  spawnLocations?: Record<string, string>;
  taskSpawnLocs?: Record<number, string | null>;
  taskSpawnTimes?: Record<number, number>;
  taskTerminateTimes?: Record<number, number>;
  taskInstrumented?: Record<number, boolean>;
  taskDumps?: Record<number, TaskDump[]>;
}): ParsedTrace {
  const toMap = <V>(r: Record<string | number, V> | undefined): Map<number, V> =>
    new Map(Object.entries(r ?? {}).map(([k, v]) => [Number(k), v]));
  return {
    spawnLocations: new Map(Object.entries(o.spawnLocations ?? {})),
    taskSpawnLocs: toMap(o.taskSpawnLocs),
    taskSpawnTimes: toMap(o.taskSpawnTimes),
    taskTerminateTimes: toMap(o.taskTerminateTimes),
    taskInstrumented: toMap(o.taskInstrumented),
    taskDumps: toMap(o.taskDumps),
  } as unknown as ParsedTrace;
}

// ── computeTaskDetailData - the collection numbers ────────────────────────

describe("computeTaskDetailData (N2 numbers, exact vs legacy)", () => {
  it("collects a task's polls across workers, sorted, with the exact counts", () => {
    const src = source({
      workerIds: [0, 1],
      pollsByWorker: {
        0: [poll(500, 600, 42), poll(100, 200, 42)],
        1: [poll(300, 400, 42), poll(1000, 1100, 7)],
      },
      wakesByTask: { 42: [wake(50, 5), wake(250, 5)] },
    });
    const trace = fakeTrace({
      spawnLocations: { loc: "src/foo/bar.rs:12" },
      taskSpawnTimes: { 42: 40 },
      taskTerminateTimes: { 42: 700 },
      taskInstrumented: { 42: true },
    });

    const data = computeTaskDetailData(src, trace, 42);
    // Polls are gathered across BOTH workers and sorted by start; the task-7
    // poll on worker 1 is excluded.
    expect(data.polls.map((p) => p.start)).toEqual([100, 300, 500]);
    expect(data.pollCount).toBe(3);
    expect(data.wakeCount).toBe(2);
    expect(data.spawnLocation).toBe("src/foo/bar.rs:12");
    expect(data.spawnTs).toBe(40);
    expect(data.terminateTs).toBe(700);
    expect(data.hasTerminate).toBe(true);
    expect(data.lifetimeNs).toBe(660);
    expect(data.isInstrumented).toBe(true);
    expect(data.workerIdCount).toBe(2);
    expect(data.hasPolls).toBe(true);
  });

  it("returns EMPTY when nothing is selected or the task has no polls (N1)", () => {
    const src = source({ workerIds: [0], pollsByWorker: { 0: [poll(0, 1, 9)] } });
    const trace = fakeTrace({});
    expect(computeTaskDetailData(src, trace, null)).toBe(EMPTY_TASK_DETAIL_DATA);
    expect(computeTaskDetailData(src, null, 9)).toBe(EMPTY_TASK_DETAIL_DATA);
    // A selected task with no matching polls hides the track.
    expect(computeTaskDetailData(src, trace, 999)).toBe(EMPTY_TASK_DETAIL_DATA);
  });

  it("omits lifetime + completion when the task never terminated", () => {
    const src = source({ workerIds: [0], pollsByWorker: { 0: [poll(10, 20, 3)] } });
    const trace = fakeTrace({ taskSpawnTimes: { 3: 5 }, taskInstrumented: { 3: true } });
    const data = computeTaskDetailData(src, trace, 3);
    expect(data.hasTerminate).toBe(false);
    expect(data.lifetimeNs).toBeNull();
    expect(data.terminateTs).toBeNull();
  });

  it("defaults taskInstrumented to true and carries the task dumps (N3/N4)", () => {
    const dumps: TaskDump[] = [{ timestamp: 15, callchain: ["a", "b"] }];
    const src = source({ workerIds: [0], pollsByWorker: { 0: [poll(10, 20, 8)] } });
    const trace = fakeTrace({ taskDumps: { 8: dumps } });
    const data = computeTaskDetailData(src, trace, 8);
    // No taskInstrumented entry => instrumented (the `?? true` default).
    expect(data.isInstrumented).toBe(true);
    expect(data.taskDumps).toEqual(dumps);
  });
});

// ── formatTaskDetailSummary - the label assembly ──────────────────────────

describe("formatTaskDetailSummary (N2 label parts)", () => {
  const base = source({ workerIds: [0], pollsByWorker: { 0: [poll(100, 200, 42), poll(300, 400, 42)] } });

  it("assembles id, location, poll/wake counts, lifetime, and completion mark", () => {
    const trace = fakeTrace({
      spawnLocations: { loc: "src/foo/bar.rs:12" },
      taskSpawnTimes: { 42: 40 },
      taskTerminateTimes: { 42: 700 },
      taskInstrumented: { 42: true },
    });
    const data = computeTaskDetailData(
      source({ workerIds: [0], pollsByWorker: { 0: base.workerSpans[0]!.polls }, wakesByTask: { 42: [wake(50, 5)] } }),
      trace,
      42,
    );
    const expected =
      `Task 0x2a — src/foo/bar.rs:12 · 2 polls · 1 wakes · lifetime ${formatHumanDuration(660)} ✓`;
    expect(formatTaskDetailSummary(data)).toBe(expected);
  });

  it("drops the wake count for an uninstrumented task (N3)", () => {
    const trace = fakeTrace({ taskInstrumented: { 42: false } });
    const data = computeTaskDetailData(base, trace, 42);
    expect(formatTaskDetailSummary(data)).toBe("Task 0x2a · 2 polls");
  });
});

// ── wakerLabelFor ─────────────────────────────────────────────────────────

describe("wakerLabelFor (N7)", () => {
  const trace = fakeTrace({
    spawnLocations: { L: "crate/mod/handler.rs:9" },
    taskSpawnLocs: { 7: "L" },
  });

  it("labels runtime/worker wakes 'io'", () => {
    expect(wakerLabelFor(0, trace, 2)).toBe("io"); // 0 => io
    expect(wakerLabelFor(1, trace, 2)).toBe("io"); // within worker-id range
    expect(wakerLabelFor(2, trace, 2)).toBe("io"); // upper bound inclusive
  });

  it("labels a tracked waker with its spawn filename", () => {
    expect(wakerLabelFor(7, trace, 2)).toBe("handler.rs:9");
  });

  it("falls back to the hex id when the waker has no spawn location", () => {
    expect(wakerLabelFor(100, trace, 2)).toBe("task 0x64");
  });
});

// ── buildTaskDetailRenderModel - geometry ─────────────────────────────────

/** A hand-built TaskDetailData (bypasses computePollWakes for geometry). */
function detailData(o: {
  polls: PollSpan[];
  pollWakes?: (PollWakeInfo | null)[];
  spawnTs?: number | null;
  terminateTs?: number | null;
  taskDumps?: TaskDump[];
}): TaskDetailData {
  const hasTerminate = o.terminateTs != null;
  return {
    ...EMPTY_TASK_DETAIL_DATA,
    taskId: 1,
    polls: o.polls,
    pollWakes: o.pollWakes ?? o.polls.map(() => null),
    pollCount: o.polls.length,
    spawnTs: o.spawnTs ?? null,
    terminateTs: o.terminateTs ?? null,
    hasTerminate,
    taskDumps: o.taskDumps ?? [],
    hasPolls: o.polls.length > 0,
  };
}

function wakeInfo(effectiveWake: number, wakerTaskId: number, label: string): PollWakeInfo {
  return { wake: wake(effectiveWake, wakerTaskId), effectiveWake, wakerLabel: label };
}

describe("buildTaskDetailRenderModel: wake bands (N6/N7)", () => {
  it("colours bands by delay severity and gates the labels on width", () => {
    // 1:1 ns->x would need a huge drawW; use a 10ms window / 1000px so the
    // three severities and both width thresholds are all exercised.
    const data = detailData({
      polls: [poll(3_000_000, 3_100_000, 1), poll(5_000_000, 5_100_000, 1), poll(8_000_000, 8_100_000, 1)],
      pollWakes: [
        wakeInfo(1_000_000, 20, "a.rs:1"), // delay 2ms  -> high, w=200
        wakeInfo(4_950_000, 21, "b.rs:2"), // delay 50us -> low,  w=5
        wakeInfo(7_500_000, 22, "c.rs:3"), // delay 500us-> mid,  w=50
      ],
    });
    const m = buildTaskDetailRenderModel({ data, viewStart: 0, viewEnd: 10_000_000, drawW: 1000 });
    expect(m.wakeBands.map((b) => b.severity)).toEqual(["high", "low", "mid"]);
    // Width thresholds: delay label when > 25px, waker label when > 40px.
    expect(m.wakeBands.map((b) => b.showDelayLabel)).toEqual([true, false, true]);
    expect(m.wakeBands.map((b) => b.showWakerLabel)).toEqual([true, false, true]);
    // A waker region exists only for the labelled (w>40) bands.
    expect(m.wakeRegions.map((r) => r.wakerTaskId)).toEqual([20, 22]);
    // Hit regions for the bands are typed "scheduled" and come first.
    expect(m.hitRegions.filter((r) => r.type === "scheduled")).toHaveLength(3);
  });
});

describe("buildTaskDetailRenderModel: poll sections (N10)", () => {
  it("renders per-poll bars when polls fit the pixel budget", () => {
    const data = detailData({ polls: [poll(100, 150, 1), poll(300, 350, 1), poll(600, 650, 1)] });
    const m = buildTaskDetailRenderModel({ data, viewStart: 0, viewEnd: 1000, drawW: 1000 });
    expect(m.coverage).toBeNull();
    expect(m.pollBars.map((b) => [b.x1, b.x2])).toEqual([
      [100, 150],
      [300, 350],
      [600, 650],
    ]);
    expect(m.visiblePolls).toBe(3);
    expect(m.hitRegions.filter((r) => r.type === "polling")).toHaveLength(3);
  });

  it("switches to a coverage histogram when polls outnumber pixels", () => {
    // 20 polls into a 10px draw area (visiblePolls > drawW).
    const polls = Array.from({ length: 20 }, (_, i) => poll(i * 50, i * 50 + 10, 1));
    const data = detailData({ polls });
    const m = buildTaskDetailRenderModel({ data, viewStart: 0, viewEnd: 1000, drawW: 10 });
    expect(m.pollBars).toHaveLength(0);
    expect(m.coverage).not.toBeNull();
    expect(m.coverage!.length).toBe(10);
    expect(m.visiblePolls).toBe(20);
    // One coarse polling hit region spans the whole band at this density.
    const polling = m.hitRegions.filter((r) => r.type === "polling");
    expect(polling).toHaveLength(1);
    expect(polling[0]!.detail).toContain("20 polls in view");
  });
});

describe("buildTaskDetailRenderModel: idle gaps (N11) + lifespan (N9)", () => {
  it("draws an idle band between polls when no wake covers the gap", () => {
    const data = detailData({ polls: [poll(100, 150, 1), poll(300, 350, 1)] });
    const m = buildTaskDetailRenderModel({ data, viewStart: 0, viewEnd: 1000, drawW: 1000 });
    expect(m.idleBands).toHaveLength(1);
    expect([m.idleBands[0]!.x1, m.idleBands[0]!.x2]).toEqual([150, 300]);
    expect(m.idleBands[0]!.hasDump).toBe(false);
    expect(m.hitRegions.some((r) => r.type === "idle")).toBe(true);
  });

  it("suppresses the idle band when the next poll's wake covers the gap", () => {
    const data = detailData({
      polls: [poll(100, 150, 1), poll(300, 350, 1)],
      // The second poll's wake starts at/behind the gap start -> wake owns it.
      pollWakes: [null, wakeInfo(140, 5, "io")],
    });
    const m = buildTaskDetailRenderModel({ data, viewStart: 0, viewEnd: 1000, drawW: 1000 });
    expect(m.idleBands).toHaveLength(0);
  });

  it("cross-hatches an idle gap that has a captured task dump", () => {
    // Dump attribution: a dump captured during poll[i-1] (ts in
    // [poll[i-1].start, poll[i].start]) describes gap i - so gap 0 has a
    // degenerate window and the dump attaches to gap 1 (between poll 1 and 2).
    const data = detailData({
      polls: [poll(100, 150, 1), poll(300, 350, 1), poll(600, 650, 1)],
      taskDumps: [{ timestamp: 200, callchain: ["x"] }],
    });
    const m = buildTaskDetailRenderModel({ data, viewStart: 0, viewEnd: 1000, drawW: 1000 });
    const dumped = m.idleBands.find((b) => b.hasDump);
    expect(dumped).toBeDefined();
    expect(dumped!.dumps).toHaveLength(1);
    const idleHit = m.hitRegions.find((r) => r.dumps !== null && r.dumps.length > 0)!;
    expect(idleHit.type).toBe("idle");
    expect(idleHit.detail).toContain("click for async stack trace");
  });

  it("places the lifespan bar with spawn/done edges inside the view", () => {
    const data = detailData({ polls: [poll(100, 200, 1)], spawnTs: 50, terminateTs: 800 });
    const m = buildTaskDetailRenderModel({ data, viewStart: 0, viewEnd: 1000, drawW: 1000 });
    expect(m.lifespan).toEqual({ x1: 50, x2: 800, showSpawn: true, showDone: true });
  });
});

// ── Hit-region ordering + status/waker lookups ────────────────────────────

describe("hit-region order + status/waker lookups (N5/N8)", () => {
  const data = detailData({
    polls: [poll(100, 150, 1), poll(400, 450, 1)],
    pollWakes: [wakeInfo(0, 9, "a.rs:1"), null], // wide wake band on poll 0
    taskDumps: [],
  });
  const m = buildTaskDetailRenderModel({ data, viewStart: 0, viewEnd: 1000, drawW: 1000 });

  it("emits hit regions in the legacy scheduled -> polling -> idle order", () => {
    const types = m.hitRegions.map((r) => r.type);
    const firstPolling = types.indexOf("polling");
    const firstIdle = types.indexOf("idle");
    expect(types.indexOf("scheduled")).toBe(0);
    expect(firstPolling).toBeGreaterThan(0);
    expect(firstIdle).toBeGreaterThan(firstPolling);
  });

  it("statusTextAt reads the first region under the pointer with its icon", () => {
    // A poll bar at x in [100,150], within the band vertically.
    const midBand = BAND_TOP + BAND_H / 2;
    expect(statusTextAt(m, 120, midBand)).toContain("⚡");
    expect(statusTextAt(m, 120, midBand)).toContain("Polling");
    // Above the band -> no region.
    expect(statusTextAt(m, 120, BAND_TOP - 5)).toBe("");
    // The scheduled band (x in [0,100]).
    expect(statusTextAt(m, 50, midBand)).toContain("⏳");
  });

  it("wakeRegionAt hits only within the waker-label strip below the band", () => {
    const inStrip = BAND_TOP + BAND_H + 10;
    expect(wakeRegionAt(m, 50, inStrip)?.wakerTaskId).toBe(9);
    // Inside the band (not the label strip) -> no waker region.
    expect(wakeRegionAt(m, 50, BAND_TOP + 5)).toBeNull();
  });

  it("returns nothing for a degenerate view (drawW <= 0 / no polls)", () => {
    const empty = buildTaskDetailRenderModel({ data, viewStart: 0, viewEnd: 1000, drawW: 0 });
    expect(empty.wakeBands).toHaveLength(0);
    expect(empty.pollBars).toHaveLength(0);
    expect(empty.hitRegions).toHaveLength(0);
    expect(hitRegionAt(empty, 10, BAND_TOP + 1)).toBeNull();
  });
});

// ── firstVisibleByEnd (windowing) ─────────────────────────────────────────

describe("firstVisibleByEnd", () => {
  it("finds the first poll whose end reaches viewStart", () => {
    const polls = [poll(0, 100, 1), poll(200, 300, 1), poll(400, 500, 1)];
    expect(firstVisibleByEnd(polls, 0)).toBe(0);
    // viewStart between poll 0's end and poll 1's start -> poll 1 is first.
    expect(firstVisibleByEnd(polls, 150)).toBe(1);
    expect(firstVisibleByEnd(polls, 450)).toBe(2);
    expect(firstVisibleByEnd(polls, 9999)).toBe(3);
  });
});
