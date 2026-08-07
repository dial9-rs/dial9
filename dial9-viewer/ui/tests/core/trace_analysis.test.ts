// Tests for trace_analysis.js: worker spans, CPU-sample attachment, wake
// indexing, scheduling delays, points of interest, flamegraph building, task
// dumps, span data, span-pane layout, poll wakes, pixel downsampling/coverage,
// bar coalescing, and allocation analysis. Most sections run against the demo
// trace; the rest use synthetic inputs.
//
// Migrated from test_trace_analysis.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale). Per T10 precedent,
// first-offender loops collect ALL offenders and assert the collection is
// empty. The frozen module is intentionally typed loosely (`any` at the
// boundary): the suite exercises runtime shapes the frozen core defines.

/* eslint-disable @typescript-eslint/no-explicit-any */

import { describe, it, expect, beforeAll } from "vitest";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

const { EVENT_TYPES, parseTrace } = require("../../trace_parser.js") as {
  EVENT_TYPES: Record<string, number>;
  parseTrace: (buf: Buffer) => Promise<any>;
};
const {
  buildWorkerSpans,
  globalQueueSeries,
  sumGlobalQueueByCycle,
  attachCpuSamples,
  buildActiveTaskTimeline,
  computeSchedulingDelays,
  filterPointsOfInterest,
  buildFlamegraphTree,
  flattenFlamegraph,
  buildFgData,
  buildSpanData,
  indexPollsByTask,
  resolveSpanTask,
  reconstructSpanSegments,
  collectDescendants,
  selectSpanRenderSet,
  computeSpanLayout,
  getTraceTimeRange,
  hasCpuProfileSamples,
  buildProcessCpuUsageSeries,
  analyzeAllocations,
  makeBarCoalescer,
  computePollWakes,
  pixelDownsampleSpans,
  pixelCoverage,
} = require("../../trace_analysis.js") as Record<string, any>;

const tracePath = fileURLToPath(
  new URL("../../public/demo-trace.bin", import.meta.url),
);

// ── Synthetic tests that need no parsed trace ─────────────────────────────

describe("trace time range / process CPU usage (synthetic)", () => {
  it("profiler-only trace range uses CPU profile samples", () => {
    const profilerOnlyTrace = {
      events: [],
      cpuSamples: [
        { timestamp: 300, source: 0, callchain: ["0x3"] },
        { timestamp: 100, source: 0, callchain: ["0x1"] },
        { timestamp: 200, source: 1, callchain: ["0x2"] },
      ],
    };

    expect(
      hasCpuProfileSamples(profilerOnlyTrace.cpuSamples),
      "CPU profile samples should make a trace displayable without runtime events",
    ).toBeTruthy();
    const range = getTraceTimeRange(
      profilerOnlyTrace.events,
      profilerOnlyTrace.cpuSamples,
    );
    expect(
      range && range.minTs === 100 && range.maxTs === 300 && range.durationNs === 200,
      `profiler-only range should come from CPU profile samples, got ${JSON.stringify(range)}`,
    ).toBeTruthy();
  });

  it("single-sample profiler-only trace range is non-zero", () => {
    const range = getTraceTimeRange(
      [],
      [{ timestamp: 100, source: 0, callchain: ["0x1"] }],
    );
    expect(
      range && range.minTs === 100 && range.maxTs === 101 && range.durationNs === 1,
      `single-sample profiler-only range should be non-zero, got ${JSON.stringify(range)}`,
    ).toBeTruthy();
  });

  it("resource-only trace range uses process resource usage events", () => {
    const range = getTraceTimeRange([], [], [
      { name: "OtherEvent", timestamp: 50, fields: {} },
      { name: "ProcessResourceUsageEvent", timestamp: 300, fields: {} },
      { name: "ProcessResourceUsageEvent", timestamp: 100, fields: {} },
    ]);
    expect(
      range && range.minTs === 100 && range.maxTs === 300 && range.durationNs === 200,
      `resource-only range should come from process resource usage events, got ${JSON.stringify(range)}`,
    ).toBeTruthy();
  });

  it("process CPU usage series derives cores and total percentage", () => {
    const series = buildProcessCpuUsageSeries(
      [
        {
          name: "ProcessResourceUsageEvent",
          timestamp: 0,
          fields: { user_cpu_ns: "100000000", system_cpu_ns: "50000000" },
        },
        {
          name: "ProcessResourceUsageEvent",
          timestamp: 1_000_000_000,
          fields: { user_cpu_ns: "500000000", system_cpu_ns: "150000000" },
        },
        {
          name: "ProcessResourceUsageEvent",
          timestamp: 2_000_000_000,
          fields: { user_cpu_ns: "1500000000", system_cpu_ns: "1150000000" },
        },
      ],
      "4",
    );
    expect(series.availableParallelism, "available parallelism should parse as 4").toBe(4);
    expect(
      series.intervals.length,
      `expected 2 CPU intervals, got ${series.intervals.length}`,
    ).toBe(2);
    const first = series.intervals[0];
    expect(
      first.userDeltaNs === 400_000_000 && first.systemDeltaNs === 100_000_000,
      `unexpected first CPU deltas: ${JSON.stringify(first)}`,
    ).toBe(true);
    expect(
      Math.abs(first.cores - 0.5),
      `expected first interval to use 0.5 cores, got ${first.cores}`,
    ).toBeLessThanOrEqual(1e-9);
    expect(
      Math.abs(first.totalPercent - 12.5),
      `expected first interval to use 12.5%, got ${first.totalPercent}`,
    ).toBeLessThanOrEqual(1e-9);
    expect(
      Math.abs(series.maxCores - 2.0),
      `expected max cores 2.0, got ${series.maxCores}`,
    ).toBeLessThanOrEqual(1e-9);
    expect(
      Math.abs(series.avgCores - 1.25),
      `expected avg cores 1.25, got ${series.avgCores}`,
    ).toBeLessThanOrEqual(1e-9);
  });

  it("process CPU usage series skips invalid pairs", () => {
    const series = buildProcessCpuUsageSeries(
      [
        {
          name: "ProcessResourceUsageEvent",
          timestamp: 0,
          fields: { user_cpu_ns: "100", system_cpu_ns: "100" },
        },
        {
          name: "ProcessResourceUsageEvent",
          timestamp: 0,
          fields: { user_cpu_ns: "200", system_cpu_ns: "200" },
        },
        {
          name: "ProcessResourceUsageEvent",
          timestamp: 10,
          fields: { user_cpu_ns: "150", system_cpu_ns: "250" },
        },
        {
          name: "ProcessResourceUsageEvent",
          timestamp: 20,
          fields: { user_cpu_ns: "300", system_cpu_ns: "350" },
        },
        { name: "OtherEvent", timestamp: 30, fields: {} },
      ],
      null,
    );
    expect(
      series.intervals.length,
      `expected only one valid CPU interval, got ${series.intervals.length}`,
    ).toBe(1);
    expect(
      series.intervals[0].start === 10 && series.intervals[0].end === 20,
      `expected valid interval [10, 20], got ${JSON.stringify(series.intervals[0])}`,
    ).toBe(true);
    expect(
      series.availableParallelism,
      "missing available parallelism should stay null",
    ).toBeNull();
  });
});

// ── Demo-trace prelude shared by the sections below ───────────────────────

let trace: any;
let workerIds: number[];
let workerSpans: any;
let queueSamples: any[];
let workerQueueSamples: any;
let maxLocalQueue: number;
let wakesByTask: any;
let wakesByWorker: any;
let cpuResult: any;
let activeTaskSamples: any[];
let schedDelays: any[];

beforeAll(async () => {
  trace = await parseTrace(readFileSync(tracePath));
  const evts = trace.events;

  const wSet = new Set<number>();
  evts.forEach((e: any) => {
    if (
      e.eventType !== EVENT_TYPES["QueueSample"] &&
      e.eventType !== EVENT_TYPES["WakeEvent"]
    )
      wSet.add(e.workerId);
  });
  workerIds = [...wSet].sort((a, b) => a - b);

  const spans = buildWorkerSpans(evts, workerIds, trace.maxTs);
  workerSpans = spans.workerSpans;
  queueSamples = spans.queueSamples;
  workerQueueSamples = spans.workerQueueSamples;
  maxLocalQueue = spans.maxLocalQueue;
  wakesByTask = spans.wakesByTask;
  wakesByWorker = spans.wakesByWorker;

  cpuResult = attachCpuSamples(trace.cpuSamples, workerSpans);

  ({ activeTaskSamples } = buildActiveTaskTimeline(
    trace.taskSpawnTimes,
    trace.taskTerminateTimes,
  ));

  schedDelays = computeSchedulingDelays(workerSpans, workerIds, wakesByTask);
}, 60_000);

// ── buildWorkerSpans ──

describe("buildWorkerSpans", () => {
  // Regression: open PollStart at trace end must not create phantom poll (#194).
  it("open PollStart at trace end is discarded (no phantom long poll)", () => {
    // Simulate a rotated segment where PollStart is the last event (no PollEnd).
    const syntheticEvents = [
      { eventType: EVENT_TYPES["PollStart"], timestamp: 1000, workerId: 0, taskId: 1, spawnLocId: null, spawnLoc: null, localQueue: 0 },
      { eventType: EVENT_TYPES["PollEnd"], timestamp: 2000, workerId: 0 },
      // This PollStart has no matching PollEnd — file rotated
      { eventType: EVENT_TYPES["PollStart"], timestamp: 3000, workerId: 0, taskId: 2, spawnLocId: null, spawnLoc: null, localQueue: 0 },
    ];
    const syntheticMaxTs = 1_000_000; // 1ms later — would create a huge phantom poll
    const result = buildWorkerSpans(syntheticEvents, [0], syntheticMaxTs);
    const polls = result.workerSpans[0].polls;
    expect(
      polls.length,
      `Expected 1 poll, got ${polls.length} — open PollStart was not discarded`,
    ).toBe(1);
    expect(
      polls[0].start === 1000 && polls[0].end === 2000,
      "Unexpected poll range",
    ).toBe(true);
  });

  it("all polls have start <= end", () => {
    const offenders: string[] = [];
    for (const w of workerIds) {
      for (const p of workerSpans[w].polls) {
        if (p.start > p.end)
          offenders.push(`Worker ${w}: poll start > end (${p.start} > ${p.end})`);
      }
    }
    expect(offenders).toEqual([]);
  });

  it("no overlapping polls on same worker", () => {
    const offenders: string[] = [];
    for (const w of workerIds) {
      const polls = workerSpans[w].polls;
      for (let i = 1; i < polls.length; i++) {
        if (polls[i].start < polls[i - 1].end)
          offenders.push(`Worker ${w}: overlapping polls at index ${i}`);
      }
    }
    expect(offenders).toEqual([]);
  });

  it("active period ratios in [0, 1]", () => {
    const offenders: string[] = [];
    for (const w of workerIds) {
      for (const a of workerSpans[w].actives) {
        if (a.ratio < 0 || a.ratio > 1)
          offenders.push(`Worker ${w}: active ratio ${a.ratio} out of [0, 1]`);
      }
    }
    expect(offenders).toEqual([]);
  });

  it("all parks have start <= end", () => {
    const offenders: string[] = [];
    for (const w of workerIds) {
      for (const p of workerSpans[w].parks) {
        if (p.start > p.end) offenders.push(`Worker ${w}: park start > end`);
      }
    }
    expect(offenders).toEqual([]);
  });

  it("queue samples exist", () => {
    // The current demo trace reports queue depth per-runtime via the
    // RuntimeMetrics side-channel, so buildWorkerSpans' legacy
    // QueueSample-derived `queueSamples` is empty. Accept either source: a
    // legacy trace populates `queueSamples`, a current one populates
    // `trace.runtimeMetrics`.
    const total = queueSamples.length + trace.runtimeMetrics.length;
    expect(total, "No queue samples (QueueSample or RuntimeMetrics)").toBeGreaterThan(0);
  });
});

// ── globalQueueSeries / sumGlobalQueueByCycle ──
// THE shared fallback both the viewer queue track and the skill scripts read,
// so a multi-runtime (RuntimeMetrics) trace and a legacy (QueueSample) trace
// yield one comparable global-queue timeline.

describe("sumGlobalQueueByCycle", () => {
  it("sums per-runtime depth per cycle timestamp and sorts by t", () => {
    const summed = sumGlobalQueueByCycle([
      { t: 20, runtimeName: "", globalQueue: 2, aliveTasks: 0 },
      { t: 10, runtimeName: "", globalQueue: 5, aliveTasks: 0 },
      { t: 10, runtimeName: "io", globalQueue: 3, aliveTasks: 0 },
    ]);
    expect(summed).toEqual([
      { t: 10, global: 8 },
      { t: 20, global: 2 },
    ]);
  });

  it("is empty for no samples", () => {
    expect(sumGlobalQueueByCycle([])).toEqual([]);
  });
});

describe("globalQueueSeries", () => {
  it("prefers summed RuntimeMetrics over the legacy queueSamples", () => {
    const trace = {
      runtimeMetrics: [
        { t: 10, runtimeName: "", globalQueue: 5, aliveTasks: 0 },
        { t: 10, runtimeName: "io", globalQueue: 3, aliveTasks: 0 },
      ],
    };
    // A legacy series is present too; RuntimeMetrics must win.
    const series = globalQueueSeries(trace, {
      queueSamples: [{ t: 10, global: 999 }],
    });
    expect(series).toEqual([{ t: 10, global: 8 }]);
  });

  it("falls back to legacy queueSamples when the trace has no RuntimeMetrics", () => {
    const legacy = [
      { t: 1, global: 4 },
      { t: 2, global: 7 },
    ];
    expect(globalQueueSeries({ runtimeMetrics: [] }, { queueSamples: legacy })).toBe(legacy);
    expect(globalQueueSeries({}, { queueSamples: legacy })).toBe(legacy);
  });

  it("recovers a non-empty global-queue series on the demo trace", () => {
    // Regression guard for the multi-runtime change: the current demo emits
    // RuntimeMetrics, not QueueSample, so buildWorkerSpans' `queueSamples` is
    // empty and only the summed path yields data (see #697).
    const spans = buildWorkerSpans(trace.events, workerIds, trace.maxTs);
    const series = globalQueueSeries(trace, spans);
    if (trace.runtimeMetrics.length > 0) {
      expect(spans.queueSamples.length).toBe(0);
      expect(series.length).toBeGreaterThan(0);
    } else {
      expect(series).toBe(spans.queueSamples);
    }
  });
});

// ── attachCpuSamples ──

describe("attachCpuSamples", () => {
  it("all attached samples fall within poll bounds", () => {
    const offenders: string[] = [];
    for (const w of workerIds) {
      for (const p of workerSpans[w].polls) {
        if (p.cpuSamples) {
          for (const s of p.cpuSamples) {
            if (s.timestamp < p.start || s.timestamp > p.end)
              offenders.push(
                `Worker ${w}: cpu sample at ${s.timestamp} outside poll [${p.start}, ${p.end}]`,
              );
          }
        }
        if (p.schedSamples) {
          for (const s of p.schedSamples) {
            if (s.timestamp < p.start || s.timestamp > p.end)
              offenders.push(
                `Worker ${w}: sched sample at ${s.timestamp} outside poll [${p.start}, ${p.end}]`,
              );
          }
        }
      }
    }
    expect(offenders).toEqual([]);
  });

  it("cpu result counts non-negative", () => {
    expect(
      cpuResult.pollsWithCpuSamples >= 0 && cpuResult.pollsWithSchedSamples >= 0,
      "Negative sample counts",
    ).toBe(true);
  });

  it("inPoll flag matches poll attachment for all samples", () => {
    // attachCpuSamples must set sample.inPoll === true iff the sample was
    // attached to a poll (the in-poll = real-blocking signal). Cross-check the
    // flag against ground truth: the set of samples actually attached to polls.
    const attached = new Set();
    for (const w of workerIds) {
      for (const p of workerSpans[w].polls) {
        for (const s of p.cpuSamples || []) attached.add(s);
        for (const s of p.schedSamples || []) attached.add(s);
      }
    }
    let mismatches = 0;
    for (const s of trace.cpuSamples) {
      if (!!s.inPoll !== attached.has(s)) mismatches++;
    }
    expect(
      mismatches,
      `sample(s) have inPoll inconsistent with poll attachment`,
    ).toBe(0);
  });

  it("off-CPU split is exhaustive (in-poll vs idle-park)", () => {
    // Splitting off-CPU samples by inPoll must partition them exactly: every
    // off-CPU sample is either in-poll (real blocking) or idle-park, never both
    // or neither. This is the invariant the BLOCKING CALLS report relies on.
    const offCpu = trace.cpuSamples.filter((s: any) => s.source === 1);
    const inPoll = offCpu.filter((s: any) => s.inPoll);
    const idle = offCpu.filter((s: any) => !s.inPoll);
    expect(
      inPoll.length + idle.length,
      `off-CPU split not exhaustive: ${inPoll.length} + ${idle.length} != ${offCpu.length}`,
    ).toBe(offCpu.length);
  });
});

// ── extractLocalQueueSamples (via buildWorkerSpans) ──

describe("extractLocalQueueSamples", () => {
  it("all local queue depths non-negative", () => {
    const offenders: string[] = [];
    for (const w of workerIds) {
      for (const s of workerQueueSamples[w]) {
        if (s.local < 0) offenders.push(`Worker ${w}: negative local queue ${s.local}`);
      }
    }
    expect(offenders).toEqual([]);
  });

  it("maxLocalQueue >= 1", () => {
    expect(maxLocalQueue, `maxLocalQueue ${maxLocalQueue} < 1`).toBeGreaterThanOrEqual(1);
  });
});

// ── buildActiveTaskTimeline ──

describe("buildActiveTaskTimeline", () => {
  it("timeline sorted by timestamp", () => {
    const offenders: number[] = [];
    for (let i = 1; i < activeTaskSamples.length; i++) {
      if (activeTaskSamples[i].t < activeTaskSamples[i - 1]!.t) offenders.push(i);
    }
    expect(offenders, "Timeline not sorted at indices").toEqual([]);
  });

  it("task counts non-negative", () => {
    const offenders = activeTaskSamples.filter((s) => s.count < 0);
    expect(offenders.map((s) => `Negative task count ${s.count}`)).toEqual([]);
  });
});

// ── indexWakeEvents (via buildWorkerSpans) ──

describe("indexWakeEvents", () => {
  it("wakesByTask arrays sorted by timestamp", () => {
    let unsorted = 0;
    for (const arr of Object.values(wakesByTask) as any[]) {
      for (let i = 1; i < arr.length; i++) {
        if (arr[i].timestamp < arr[i - 1].timestamp) unsorted++;
      }
    }
    expect(unsorted, "wakesByTask not sorted").toBe(0);
  });

  it("wakesByWorker arrays sorted by timestamp", () => {
    let unsorted = 0;
    for (const arr of Object.values(wakesByWorker) as any[]) {
      for (let i = 1; i < arr.length; i++) {
        if (arr[i].timestamp < arr[i - 1].timestamp) unsorted++;
      }
    }
    expect(unsorted, "wakesByWorker not sorted").toBe(0);
  });

  it("wake counts consistent across both indexes", () => {
    let taskTotal = 0;
    for (const arr of Object.values(wakesByTask) as any[]) taskTotal += arr.length;
    let workerTotal = 0;
    for (const arr of Object.values(wakesByWorker) as any[]) workerTotal += arr.length;
    expect(
      taskTotal,
      `wakesByTask total ${taskTotal} != wakesByWorker total ${workerTotal}`,
    ).toBe(workerTotal);
  });
});

// ── computeSchedulingDelays ──

describe("computeSchedulingDelays", () => {
  it("all delays positive", () => {
    const offenders = schedDelays.filter((sd) => sd.delay <= 0);
    expect(offenders.map((sd) => `Non-positive delay: ${sd.delay}`)).toEqual([]);
  });

  it("all delays < 1s", () => {
    const offenders = schedDelays.filter((sd) => sd.delay >= 1e9);
    expect(offenders.map((sd) => `Delay >= 1s: ${sd.delay}`)).toEqual([]);
  });

  it("wakeTime < pollTime for all delays", () => {
    const offenders = schedDelays.filter((sd) => sd.wakeTime >= sd.pollTime);
    expect(
      offenders.map((sd) => `wakeTime ${sd.wakeTime} >= pollTime ${sd.pollTime}`),
    ).toEqual([]);
  });

  it("schedDelays sorted by wakeTime", () => {
    let unsorted = 0;
    for (let i = 1; i < schedDelays.length; i++) {
      if (schedDelays[i].wakeTime < schedDelays[i - 1]!.wakeTime) unsorted++;
    }
    expect(unsorted, "schedDelays not sorted by wakeTime").toBe(0);
  });

  it("mid-poll wake adjusted to containing poll's end (binary search)", () => {
    // Bug-1 regression guard: the mid-poll wake adjustment. If a wake lands
    // inside an earlier poll of the same task, the delay must be measured from
    // that poll's end, not the wake itself. The demo trace may not exercise
    // this branch, so drive it with a synthetic two-poll task.
    const ws = {
      0: {
        polls: [
          { taskId: 1, start: 100, end: 200 },
          { taskId: 1, start: 500, end: 600 },
        ],
      },
    };
    // Wake at t=150 lands inside the first poll [100,200].
    const wakes = { 1: [{ timestamp: 150, wakerTaskId: 9 }] };
    const r = computeSchedulingDelays(ws, [0], wakes);
    // For poll #2 (start=500), effectiveWake should snap to poll #1.end = 200,
    // giving delay = 500 - 200 = 300 (not 500 - 150 = 350).
    expect(r.length, `expected 1 sched delay, got ${r.length}`).toBe(1);
    expect(
      r[0].wakeTime === 200 && r[0].delay === 300,
      `mid-poll wake not adjusted: wakeTime=${r[0].wakeTime} delay=${r[0].delay} (expected 200/300)`,
    ).toBe(true);
  });

  it("wake in inter-poll gap left unadjusted", () => {
    // Counterpart: a wake that falls in the gap between polls (inside no poll)
    // must NOT be adjusted — delay is measured straight from the wake.
    const ws = {
      0: {
        polls: [
          { taskId: 1, start: 100, end: 200 },
          { taskId: 1, start: 500, end: 600 },
        ],
      },
    };
    const wakes = { 1: [{ timestamp: 300, wakerTaskId: 9 }] }; // in gap (200,500)
    const r = computeSchedulingDelays(ws, [0], wakes);
    expect(r.length, `expected 1 sched delay, got ${r.length}`).toBe(1);
    expect(
      r[0].wakeTime === 300 && r[0].delay === 200,
      `gap wake wrongly adjusted: wakeTime=${r[0].wakeTime} delay=${r[0].delay} (expected 300/200)`,
    ).toBe(true);
  });
});

// ── filterPointsOfInterest ──

describe("filterPointsOfInterest", () => {
  it("long-poll filter: results all > 1ms", () => {
    const pois = filterPointsOfInterest("long-poll", workerSpans, workerIds, schedDelays, {
      hasSchedWait: trace.hasSchedWait,
    });
    expect(pois.length, "No long-poll points of interest found").toBeGreaterThan(0);
    const offenders = pois.filter((p: any) => p.type !== "long-poll" || p.value <= 1);
    expect(
      offenders.map((p: any) => `type=${p.type} value=${p.value}`),
      "long-poll results with wrong type or value <= 1ms",
    ).toEqual([]);
  });

  it("cpu-sampled filter: results all with samples", () => {
    const pois = filterPointsOfInterest("cpu-sampled", workerSpans, workerIds, schedDelays, {
      hasSchedWait: trace.hasSchedWait,
    });
    expect(pois.length, "No cpu-sampled points of interest found").toBeGreaterThan(0);
    const offenders = pois.filter((p: any) => p.type !== "cpu-sampled" || p.value <= 0);
    expect(
      offenders.map((p: any) => `type=${p.type} value=${p.value}`),
      "cpu-sampled results with wrong type or value <= 0",
    ).toEqual([]);
  });

  it("wake-delay filter: results all > 100µs", () => {
    const pois = filterPointsOfInterest("wake-delay", workerSpans, workerIds, schedDelays, {
      hasSchedWait: trace.hasSchedWait,
    });
    expect(pois.length, "No wake-delay points of interest found").toBeGreaterThan(0);
    const offenders = pois.filter((p: any) => p.type !== "wake-delay" || p.value <= 100);
    expect(
      offenders.map((p: any) => `type=${p.type} value=${p.value}`),
      "wake-delay results with wrong type or value <= 100µs",
    ).toEqual([]);
  });

  it("sortByWorst produces descending order", () => {
    const pois = filterPointsOfInterest("long-poll", workerSpans, workerIds, schedDelays, {
      hasSchedWait: trace.hasSchedWait,
      sortByWorst: true,
    });
    let unsorted = 0;
    for (let i = 1; i < pois.length; i++) {
      if (pois[i].value > pois[i - 1].value) unsorted++;
    }
    expect(unsorted, "sortByWorst not descending").toBe(0);
  });
});

// ── buildFlamegraphTree / flattenFlamegraph ──

describe("flamegraph", () => {
  it("root count matches sample count", () => {
    const cpuSamples = trace.cpuSamples.filter((s: any) => s.source !== 1);
    expect(cpuSamples.length, "No CPU samples found").toBeGreaterThan(0);

    const root = buildFlamegraphTree(cpuSamples, trace.callframeSymbols);
    expect(
      root.count,
      `Root count ${root.count} != sample count ${cpuSamples.length}`,
    ).toBe(cpuSamples.length);
  });

  it("flattenFlamegraph nodes within [0,1) with positive widths", () => {
    const cpuSamples = trace.cpuSamples.filter((s: any) => s.source !== 1);
    expect(cpuSamples.length, "No CPU samples found").toBeGreaterThan(0);

    const root = buildFlamegraphTree(cpuSamples, trace.callframeSymbols);
    const { nodes, maxDepth } = flattenFlamegraph(root, cpuSamples.length);
    const offenders: string[] = [];
    for (const n of nodes) {
      if (n.x < 0 || n.x >= 1) offenders.push(`Node x=${n.x} out of [0, 1)`);
      if (n.w <= 0) offenders.push(`Node w=${n.w} <= 0`);
    }
    expect(offenders).toEqual([]);
    expect(maxDepth, `maxDepth ${maxDepth} < 0`).toBeGreaterThanOrEqual(0);
  });

  it("buildFgData counts totalSamples", () => {
    const cpuSamples = trace.cpuSamples.filter((s: any) => s.source !== 1);
    expect(cpuSamples.length, "No CPU samples found").toBeGreaterThan(0);

    const data = buildFgData(cpuSamples, trace.callframeSymbols);
    expect(data, "buildFgData returned null for non-empty samples").toBeTruthy();
    expect(
      data.totalSamples,
      `totalSamples ${data.totalSamples} != ${cpuSamples.length}`,
    ).toBe(cpuSamples.length);
  });

  it("buildFgData returns null for empty samples", () => {
    const data = buildFgData([], trace.callframeSymbols);
    expect(data).toBeNull();
  });

  // Inlined frames: when callframeSymbols.get(addr) returns an array, per
  // blazesym the array is ordered [outermost, ..., innermost]. entry[0] is the
  // real function at the address; entry[i>0] are inlined callees so the call
  // chain goes entry[0] -> entry[1] -> entry[2]. The flamegraph tree must
  // descend in that same order (outermost as parent, innermost as leaf).
  it("inlined frames expand outermost->innermost as parent->child", () => {
    const callframeSymbols = new Map([
      [
        "0x1000",
        [
          { symbol: "outer_fn", location: "outer.rs:10" },
          { symbol: "mid_fn", location: "mid.rs:20" },
          { symbol: "leaf_fn", location: "leaf.rs:30" },
        ],
      ],
    ]);
    const samples = [{ callchain: ["0x1000"], workerId: 0 }];
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    expect(tree.children.size, `root has ${tree.children.size} children, expected 1`).toBe(1);
    const outer = [...tree.children.values()][0] as any;
    expect(
      outer.fullName.includes("outer_fn"),
      `child of root is "${outer.fullName}", expected "outer_fn"`,
    ).toBe(true);
    expect(outer.children.size, `outer has ${outer.children.size} children, expected 1`).toBe(1);
    const mid = [...outer.children.values()][0] as any;
    expect(
      mid.fullName.includes("mid_fn"),
      `child of outer is "${mid.fullName}", expected "mid_fn"`,
    ).toBe(true);
    const leaf = [...mid.children.values()][0] as any;
    expect(
      leaf.fullName.includes("leaf_fn"),
      `child of mid is "${leaf.fullName}", expected "leaf_fn"`,
    ).toBe(true);
    expect(
      leaf.self,
      `leaf.self = ${leaf.self}, expected 1 (innermost frame is where the sample lands)`,
    ).toBe(1);
  });

  // The inline-expansion code must not crash when an address maps to an array
  // with nullish elements (can happen with sparse SymbolTableEntry events or
  // when a child inline is resolved before its parent frame).
  it("sparse inline arrays do not produce phantom tree levels", () => {
    // arr[0] present, arr[1] undefined, arr[2] present. The iteration should
    // skip the undefined slot rather than creating a "(unknown)" level.
    const sparse = new Array(3);
    sparse[0] = { symbol: "outer_fn", location: null };
    sparse[2] = { symbol: "leaf_fn", location: null };
    const callframeSymbols = new Map([["0x2000", sparse]]);
    const samples = [{ callchain: ["0x2000"], workerId: 0 }];
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    // Expected: (all) -> outer_fn -> leaf_fn (sparse slot skipped)
    const outer = [...tree.children.values()][0] as any;
    expect(
      outer && outer.fullName.includes("outer_fn"),
      `expected outer_fn child, got ${outer && outer.fullName}`,
    ).toBeTruthy();
    expect(
      outer.children.size,
      `outer has ${outer.children.size} children, expected 1 (sparse slot should be skipped)`,
    ).toBe(1);
    const leaf = [...outer.children.values()][0] as any;
    expect(
      leaf.fullName.includes("leaf_fn"),
      `expected leaf_fn after outer_fn, got ${leaf.fullName}`,
    ).toBe(true);
  });

  // An address that is not present in callframeSymbols should still produce
  // a single-level child using the raw address as the key (so unresolved
  // traces remain visible rather than collapsing).
  it("unresolved addresses still produce a single tree level", () => {
    const callframeSymbols = new Map(); // empty — address resolves to undefined
    const samples = [{ callchain: ["0x3000"], workerId: 0 }];
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    expect(
      tree.children.size,
      `root has ${tree.children.size} children for single unresolved address`,
    ).toBe(1);
    const node = [...tree.children.values()][0] as any;
    expect(node.self, `unresolved node.self = ${node.self}, expected 1`).toBe(1);
  });

  it("weighted samples accumulate count, self, allocCount, selfAllocCount", () => {
    const callframeSymbols = new Map([
      ["0xA", [{ symbol: "alloc_fn", location: "alloc.rs:1" }]],
      ["0xB", [{ symbol: "caller_fn", location: "caller.rs:1" }]],
    ]);
    // callchain is leaf-first: [leaf, ..., root]. Reversed internally to root→leaf.
    const samples = [
      { callchain: ["0xA", "0xB"], weight: 1000, allocWeight: 2 },
      { callchain: ["0xA", "0xB"], weight: 500, allocWeight: 1.5 },
      { callchain: ["0xA"], weight: 200, allocWeight: 1 },
    ];
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    expect(tree.count, `root.count = ${tree.count}, expected 1700`).toBe(1700);
    expect(tree.allocCount, `root.allocCount = ${tree.allocCount}, expected 4.5`).toBe(4.5);
    // First two samples: caller_fn -> alloc_fn. Third: alloc_fn only.
    const caller = tree.children.get("caller_fn");
    expect(caller, "expected caller_fn as child of root").toBeTruthy();
    expect(caller.count, `caller.count = ${caller.count}, expected 1500`).toBe(1500);
    expect(caller.self, `caller.self = ${caller.self}, expected 0`).toBe(0);
    const alloc = caller.children.get("alloc_fn");
    expect(alloc, "expected alloc_fn as child of caller_fn").toBeTruthy();
    expect(alloc.count, `alloc.count = ${alloc.count}, expected 1500`).toBe(1500);
    expect(alloc.self, `alloc.self = ${alloc.self}, expected 1500`).toBe(1500);
    expect(
      alloc.selfAllocCount,
      `alloc.selfAllocCount = ${alloc.selfAllocCount}, expected 3.5`,
    ).toBe(3.5);
    // Third sample: alloc_fn is root-level child
    const allocDirect = tree.children.get("alloc_fn");
    expect(allocDirect, "expected alloc_fn as direct child of root for single-frame sample").toBeTruthy();
    expect(allocDirect.count, `allocDirect.count = ${allocDirect.count}, expected 200`).toBe(200);
    expect(allocDirect.self, `allocDirect.self = ${allocDirect.self}, expected 200`).toBe(200);
    expect(
      allocDirect.selfAllocCount,
      `allocDirect.selfAllocCount = ${allocDirect.selfAllocCount}, expected 1`,
    ).toBe(1);
  });

  it("unweighted samples default to weight=1, no allocCount fields", () => {
    const callframeSymbols = new Map([
      ["0xC", [{ symbol: "cpu_fn", location: "cpu.rs:1" }]],
    ]);
    const samples = [{ callchain: ["0xC"] }, { callchain: ["0xC"] }];
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    expect(tree.count, `root.count = ${tree.count}, expected 2`).toBe(2);
    const node = tree.children.get("cpu_fn");
    expect(node.count, `node.count = ${node.count}, expected 2`).toBe(2);
    expect(node.self, `node.self = ${node.self}, expected 2`).toBe(2);
    expect(
      node.allocCount == null,
      "node.allocCount should be undefined for unweighted samples",
    ).toBe(true);
  });
});

// ── TaskDumpEvent parsing (verified against the demo trace) ──

describe("taskDumps", () => {
  it("trace.taskDumps is a Map", () => {
    expect(trace.taskDumps, "trace.taskDumps should be a Map").toBeTruthy();
    expect(
      trace.taskDumps instanceof Map,
      "trace.taskDumps should be an instance of Map",
    ).toBe(true);
  });

  it("all taskDumps arrays are sorted by timestamp", () => {
    // Every value in taskDumps is an array sorted by timestamp — the renderer
    // relies on this for its O(n) sweep across idle gaps.
    const offenders: string[] = [];
    for (const [tid, dumps] of trace.taskDumps) {
      for (let i = 1; i < dumps.length; i++) {
        if (dumps[i].timestamp < dumps[i - 1].timestamp) {
          offenders.push(`taskDumps for task ${tid} not sorted (index ${i})`);
        }
      }
    }
    expect(offenders).toEqual([]);
  });

  it("taskDumps have {timestamp, callchain} shape with hex-string addresses", () => {
    // Each dump is {timestamp, callchain} where callchain is an array of hex address strings.
    const offenders: string[] = [];
    for (const [tid, dumps] of trace.taskDumps) {
      for (const d of dumps) {
        if (typeof d.timestamp !== "number")
          offenders.push(`dump.timestamp for task ${tid} is ${typeof d.timestamp}`);
        if (!Array.isArray(d.callchain))
          offenders.push(`dump.callchain for task ${tid} is not an array`);
        else
          for (const addr of d.callchain) {
            if (typeof addr !== "string" || !addr.startsWith("0x")) {
              offenders.push(`dump.callchain entry ${addr} not a hex string`);
            }
          }
        break; // sample one per task is enough
      }
    }
    expect(offenders).toEqual([]);
  });

  it("all taskDump task IDs refer to tasks in taskSpawnTimes", () => {
    // Every task ID that has a dump should be a known spawned task (no orphans).
    const offenders: string[] = [];
    for (const tid of trace.taskDumps.keys()) {
      if (!trace.taskSpawnTimes.has(tid)) {
        offenders.push(`task ${tid} has taskDumps but is not in taskSpawnTimes`);
      }
    }
    expect(offenders).toEqual([]);
  });
});

// ── buildSpanData ──

describe("buildSpanData", () => {
  it("pairs enter/exit into spans", () => {
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "handle_request", fields: { user_id: "42" } } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "redis_get", fields: { key: "foo" } } },
      { name: "SpanExitEvent", timestamp: 1200, fields: { worker_id: 0, span_id: 2, span_name: "redis_get", fields: { key: "foo" } } },
      { name: "SpanExitEvent", timestamp: 1300, fields: { worker_id: 0, span_id: 1, span_name: "handle_request", fields: { user_id: "42" } } },
    ];
    const { allSpans, spanMeta } = buildSpanData(customEvents);
    expect(allSpans.length, `Expected 2 spans, got ${allSpans.length}`).toBe(2);
    const redis = allSpans.find((s: any) => s.spanName === "redis_get");
    const handle = allSpans.find((s: any) => s.spanName === "handle_request");
    expect(redis && handle, "Missing expected spans").toBeTruthy();
    expect(redis.start === 1100 && redis.end === 1200, "redis_get timing wrong").toBe(true);
    expect(redis.segments.length, `Expected 1 segment, got ${redis.segments.length}`).toBe(1);
    expect(redis.segments[0].workerId, "segment workerId wrong").toBe(0);
    expect(spanMeta.has("1") && spanMeta.has("2"), "spanMeta missing entries").toBe(true);
    // Verify sorted by start time
    expect(allSpans[0].start <= allSpans[1].start, "Spans not sorted by start time").toBe(true);
  });

  it("turns a normalized single event into a complete span", () => {
    const workerSpans = {
      0: { polls: [{ start: 900, end: 1100, taskId: 42 }] },
      1: { polls: [{ start: 4000, end: 4100, taskId: 42 }] },
    };
    const customEvents = [{
      name: "any-producer:RequestMetrics",
      timestamp: 9000,
      fields: { arbitrary: "wire fields are not interpreted here" },
      singleEventSpan: {
        start: 1000,
        end: 9000,
        name: "QueryMetric",
        spanType: "any-producer",
        threadId: 77,
        taskId: 42,
        workerId: null,
        fields: { Operation: "QueryMetric", Success: true },
        units: { Success: "count" },
      },
    }];

    const { allSpans, unmatchedSpans } = buildSpanData(
      customEvents,
      workerSpans,
      new Map([[77, [{ timestamp: 500, workerId: 0 }]]]),
    );
    expect(allSpans).toHaveLength(1);
    expect(unmatchedSpans).toHaveLength(0);
    const span = allSpans[0];
    expect(span.spanId).toBe("single-event:0");
    expect(span.spanName).toBe("QueryMetric");
    expect(span.start).toBe(1000);
    expect(span.end).toBe(9000);
    expect(span.taskId).toBe(42);
    expect(span.parentSpanId).toBeNull();
    expect(span.fields).toEqual({ Operation: "QueryMetric", Success: true });
    expect(span.spanType).toBe("any-producer");
    expect(span.units).toEqual({ Success: "count" });
    expect(span.segments).toEqual([
      { start: 1000, end: 1100, workerId: 0 },
      { start: 4000, end: 4100, workerId: 1 },
    ]);
    expect(span.activeNs).toBe(200);

    // A direct caller may not have built worker spans. The captured task id
    // must not make that path dereference a missing poll index.
    const withoutPolls = buildSpanData(customEvents).allSpans[0];
    expect(withoutPolls.taskId).toBe(42);
    expect(withoutPolls.segments).toEqual([]);
    expect(withoutPolls.activeNs).toBe(0);
  });

  it("resolves a remapped thread at the single-event span start", () => {
    const workerSpans = {
      0: { polls: [{ start: 100, end: 200, taskId: 1 }] },
      1: { polls: [{ start: 300, end: 400, taskId: 2 }] },
    };
    const customEvents = [{
      name: "producer:Work",
      timestamp: 390,
      fields: {},
      singleEventSpan: {
        start: 310,
        end: 390,
        name: "work",
        spanType: "producer",
        threadId: 77,
        taskId: null,
        workerId: null,
        fields: {},
        units: null,
      },
    }];
    const bindings = new Map([[
      77,
      [
        { timestamp: 50, workerId: 0 },
        { timestamp: 250, workerId: 1 },
      ],
    ]]);

    expect(buildSpanData(customEvents, workerSpans, bindings).allSpans[0].taskId)
      .toBe(2);
  });

  it("does not derive a task from a thread inside a block-in-place handoff gap", () => {
    const workerSpans = {
      0: { polls: [{ start: 100, end: 200, taskId: 1 }] },
    };
    // `any` at the boundary (see the file header): the second half of this test
    // reassigns workerId, which a literal `null` would otherwise pin to `null`.
    const customEvents: any[] = [{
      name: "producer:Work",
      timestamp: 170,
      fields: {},
      singleEventSpan: {
        start: 110,
        end: 170,
        name: "work",
        spanType: "producer",
        threadId: 77,
        taskId: null,
        workerId: null,
        fields: {},
        units: null,
      },
    }];
    const bindings = new Map([[77, [{ timestamp: 50, workerId: 0 }]]]);
    const gaps = [{
      workerId: 0,
      fromTid: 77,
      toTid: 88,
      startNs: 50,
      endNs: 200,
    }];

    const span = buildSpanData(customEvents, workerSpans, bindings, gaps).allSpans[0];
    expect(span.taskId).toBeNull();
    expect(span.segments).toEqual([]);
    expect(span.activeNs).toBe(0);

    customEvents[0]!.singleEventSpan.workerId = 0;
    const directlyAnnotated = buildSpanData(customEvents, workerSpans, bindings, gaps)
      .allSpans[0];
    expect(directlyAnnotated.taskId).toBeNull();
    expect(directlyAnnotated.segments).toEqual([]);
  });

  it("does not infer spans from metrique schema names", () => {
    expect(buildSpanData([{
      name: "metrique:Unannotated",
      timestamp: 1000,
      fields: { Operation: "not a span" },
    }]).allSpans).toHaveLength(0);
  });

  it("parent span IDs preserved", () => {
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 10, parent_span_id: null, span_name: "root", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 20, parent_span_id: 10, span_name: "child", fields: {} } },
      { name: "SpanExitEvent", timestamp: 1200, fields: { worker_id: 0, span_id: 20, span_name: "child", fields: {} } },
      { name: "SpanExitEvent", timestamp: 1300, fields: { worker_id: 0, span_id: 10, span_name: "root", fields: {} } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    const child = allSpans.find((s: any) => s.spanName === "child");
    expect(child.parentSpanId, `Expected parentSpanId="10", got ${child.parentSpanId}`).toBe("10");
    const root = allSpans.find((s: any) => s.spanName === "root");
    expect(root.parentSpanId, `Expected root parentSpanId=null, got ${root.parentSpanId}`).toBeNull();
  });

  it("empty input produces empty output", () => {
    const { allSpans, spanMeta } = buildSpanData([]);
    expect(allSpans.length, "Expected empty allSpans").toBe(0);
    expect(spanMeta.size, "Expected empty spanMeta").toBe(0);
  });

  it("depth computed correctly for 3-level nesting", () => {
    // Three levels of nesting via explicit parent
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "root", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "mid", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1200, fields: { worker_id: 0, span_id: 3, parent_span_id: 2, span_name: "leaf", fields: {} } },
      { name: "SpanExitEvent", timestamp: 1300, fields: { worker_id: 0, span_id: 3, span_name: "leaf", fields: {} } },
      { name: "SpanExitEvent", timestamp: 1400, fields: { worker_id: 0, span_id: 2, span_name: "mid", fields: {} } },
      { name: "SpanExitEvent", timestamp: 1500, fields: { worker_id: 0, span_id: 1, span_name: "root", fields: {} } },
    ];
    const { allSpans, maxDepth } = buildSpanData(customEvents);
    const root = allSpans.find((s: any) => s.spanName === "root");
    const mid = allSpans.find((s: any) => s.spanName === "mid");
    const leaf = allSpans.find((s: any) => s.spanName === "leaf");
    expect(root.depth, `root depth=${root.depth}, expected 0`).toBe(0);
    expect(mid.depth, `mid depth=${mid.depth}, expected 1`).toBe(1);
    expect(leaf.depth, `leaf depth=${leaf.depth}, expected 2`).toBe(2);
    expect(maxDepth, `maxDepth=${maxDepth}, expected 2`).toBe(2);
  });

  it("cyclic parent chain does not stack overflow", () => {
    // Cyclic parent chain: A -> B -> A (should not stack overflow)
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: 2, span_name: "a", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "b", fields: {} } },
      { name: "SpanExitEvent", timestamp: 1200, fields: { worker_id: 0, span_id: 2, span_name: "b", fields: {} } },
      { name: "SpanExitEvent", timestamp: 1300, fields: { worker_id: 0, span_id: 1, span_name: "a", fields: {} } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    expect(allSpans.length, "Expected 2 spans").toBe(2);
    // Just verify it didn't crash; depths may be arbitrary due to cycle
  });

  it("recycled span IDs produce separate intervals (and group by id)", () => {
    // Span ID 1 used first as "alpha", closed, then recycled as "beta"
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "alpha", fields: {} } },
      { name: "SpanExitEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 1, span_name: "alpha", fields: {} } },
      { name: "SpanCloseEvent", timestamp: 1150, fields: { span_id: 1 } },
      // Same span_id reused with different name
      { name: "SpanEnterEvent", timestamp: 2000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "beta", fields: {} } },
      { name: "SpanExitEvent", timestamp: 2100, fields: { worker_id: 0, span_id: 1, span_name: "beta", fields: {} } },
      { name: "SpanCloseEvent", timestamp: 2150, fields: { span_id: 1 } },
      // Child of the recycled span
      { name: "SpanEnterEvent", timestamp: 3000, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "child", fields: {} } },
      { name: "SpanExitEvent", timestamp: 3100, fields: { worker_id: 0, span_id: 2, span_name: "child", fields: {} } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    expect(allSpans.length, `Expected 3 spans, got ${allSpans.length}`).toBe(3);
    const alpha = allSpans.find((s: any) => s.spanName === "alpha");
    const beta = allSpans.find((s: any) => s.spanName === "beta");
    expect(alpha && beta, "Missing alpha or beta span").toBeTruthy();
    expect(
      alpha.start === 1000 && beta.start === 2000,
      "Span intervals not distinct",
    ).toBe(true);
    // The viewer's per-lane highlight loop relies on grouping allSpans by
    // spanId into a multimap (spansByIdAll) and lighting up EVERY instance —
    // not a single last-wins entry. Assert that recycled id 1 indeed yields
    // two grouped instances, so the highlight stays correct under recycling.
    const byId = new Map<unknown, any[]>();
    for (const s of allSpans) {
      let b = byId.get(s.spanId);
      if (!b) {
        b = [];
        byId.set(s.spanId, b);
      }
      b.push(s);
    }
    // spanId is keyed exactly as stored on the span objects (a string here),
    // the same value the viewer puts in selectedSpanIds — so the multimap key
    // is alpha.spanId, not a numeric literal.
    expect(
      (byId.get(alpha.spanId) || []).length,
      `Expected id ${JSON.stringify(alpha.spanId)} to group 2 recycled spans`,
    ).toBe(2);
    expect(alpha.spanId, "Recycled spans should share the same spanId key").toBe(beta.spanId);
  });

  it("per-callsite schema with typed fields parsed correctly", () => {
    // New format: schema names are "SpanEnter:target::name:file:line"
    // User fields are top-level (not nested in a "fields" StringMap)
    const customEvents = [
      { name: "SpanEnter:myapp::handle:src/main.rs:10", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "handle", request_id: "abc-123" } },
      { name: "SpanExit:myapp::handle:src/main.rs:10", timestamp: 1100, fields: { worker_id: 0, span_id: 1, span_name: "handle", request_id: "abc-123" } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    expect(
      allSpans && allSpans.length === 1,
      `Expected 1 span, got ${allSpans?.length}`,
    ).toBeTruthy();
    expect(
      allSpans[0].spanName,
      `Expected span name 'handle', got '${allSpans[0].spanName}'`,
    ).toBe("handle");
    expect(
      allSpans[0].fields.request_id,
      `Expected request_id='abc-123', got '${allSpans[0].fields.request_id}'`,
    ).toBe("abc-123");
    // Base fields should NOT appear in the user fields
    expect(
      allSpans[0].fields.worker_id,
      "worker_id should not be in user fields",
    ).toBeFalsy();
    expect(
      allSpans[0].fields.span_name,
      "span_name should not be in user fields",
    ).toBeFalsy();
  });

  it("unmatched spans (enter without exit) detected correctly", () => {
    const customEvents = [
      { name: "SpanEnter:app::a:f:1", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "a" } },
      { name: "SpanExit:app::a:f:1", timestamp: 1100, fields: { worker_id: 0, span_id: 1, span_name: "a" } },
      // This enter has no matching exit (trace ended mid-span)
      { name: "SpanEnter:app::b:f:2", timestamp: 1200, fields: { worker_id: 0, span_id: 2, parent_span_id: null, span_name: "b" } },
    ];
    const { allSpans, unmatchedSpans } = buildSpanData(customEvents);
    expect(allSpans.length, `Expected 1 matched span, got ${allSpans.length}`).toBe(1);
    expect(
      unmatchedSpans && unmatchedSpans.length === 1,
      `Expected 1 unmatched span, got ${unmatchedSpans?.length}`,
    ).toBeTruthy();
    expect(
      unmatchedSpans[0].spanName,
      `Expected unmatched span 'b', got '${unmatchedSpans[0].spanName}'`,
    ).toBe("b");
    expect(
      unmatchedSpans[0].spanId,
      `Expected unmatched spanId "2", got ${unmatchedSpans[0].spanId}`,
    ).toBe("2");
  });

  it("childrenByParent index built correctly", () => {
    // Root r1 has children c1, c2. c1 has grandchild g1. r2 is childless.
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 100, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "r1" } },
      { name: "SpanEnterEvent", timestamp: 110, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "c1" } },
      { name: "SpanEnterEvent", timestamp: 120, fields: { worker_id: 0, span_id: 3, parent_span_id: 2, span_name: "g1" } },
      { name: "SpanExitEvent", timestamp: 130, fields: { worker_id: 0, span_id: 3, span_name: "g1" } },
      { name: "SpanExitEvent", timestamp: 140, fields: { worker_id: 0, span_id: 2, span_name: "c1" } },
      { name: "SpanEnterEvent", timestamp: 150, fields: { worker_id: 0, span_id: 4, parent_span_id: 1, span_name: "c2" } },
      { name: "SpanExitEvent", timestamp: 160, fields: { worker_id: 0, span_id: 4, span_name: "c2" } },
      { name: "SpanExitEvent", timestamp: 170, fields: { worker_id: 0, span_id: 1, span_name: "r1" } },
      { name: "SpanEnterEvent", timestamp: 200, fields: { worker_id: 0, span_id: 5, parent_span_id: null, span_name: "r2" } },
      { name: "SpanExitEvent", timestamp: 210, fields: { worker_id: 0, span_id: 5, span_name: "r2" } },
    ];
    const { childrenByParent } = buildSpanData(customEvents);
    expect(childrenByParent, "childrenByParent not exposed from buildSpanData").toBeTruthy();
    const roots = childrenByParent.get(null) || [];
    expect(
      roots.includes("1") && roots.includes("5"),
      `Roots should include "1" and "5", got ${[...roots]}`,
    ).toBe(true);
    const c1Children = childrenByParent.get("1") || [];
    expect(
      c1Children.includes("2") && c1Children.includes("4"),
      `r1 should have children "2" and "4", got ${[...c1Children]}`,
    ).toBe(true);
    const g1Children = childrenByParent.get("2") || [];
    expect(
      g1Children.includes("3"),
      `c1 should have child "3", got ${[...g1Children]}`,
    ).toBe(true);
    // Childless spans should have no entry (or empty array)
    const r2Children = childrenByParent.get("5") || [];
    expect(r2Children.length, `r2 should be childless, got ${[...r2Children]}`).toBe(0);
  });

  it("multiple polls grouped into single span with segments", () => {
    // A span entered/exited multiple times (async future polled 3 times with sleep gap)
    const customEvents = [
      { name: "SpanEnter:app::f:f:1", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "my_fn" } },
      { name: "SpanExit:app::f:f:1", timestamp: 1500, fields: { worker_id: 0, span_id: 1, span_name: "my_fn" } },
      { name: "SpanEnter:app::f:f:1", timestamp: 100000, fields: { worker_id: 1, span_id: 1, parent_span_id: null, span_name: "my_fn" } },
      { name: "SpanExit:app::f:f:1", timestamp: 100200, fields: { worker_id: 1, span_id: 1, span_name: "my_fn" } },
      { name: "SpanEnter:app::f:f:1", timestamp: 100300, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "my_fn" } },
      { name: "SpanExit:app::f:f:1", timestamp: 100400, fields: { worker_id: 0, span_id: 1, span_name: "my_fn" } },
      { name: "SpanCloseEvent", timestamp: 100500, fields: { span_id: 1 } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    expect(allSpans.length, `Expected 1 span, got ${allSpans.length}`).toBe(1);
    const s = allSpans[0];
    expect(s.segments.length, `Expected 3 segments, got ${s.segments.length}`).toBe(3);
    expect(s.start, `Expected start=1000, got ${s.start}`).toBe(1000);
    expect(s.end, `Expected end=100400, got ${s.end}`).toBe(100400);
    // activeNs = 500 + 200 + 100 = 800
    expect(s.activeNs, `Expected activeNs=800, got ${s.activeNs}`).toBe(800);
    // Workers: polled on both 0 and 1
    const workers = [...new Set(s.segments.map((seg: any) => seg.workerId))].sort();
    expect(
      workers.length === 2 && workers[0] === 0 && workers[1] === 1,
      `Expected workers [0,1], got ${workers}`,
    ).toBe(true);
  });

  it("out-of-order events sorted; duplicate enter on different worker ignored", () => {
    // Events arrive out of order across workers — buildSpanData sorts by timestamp.
    // Also tests the defensive guard: span 1 enters on worker 0, then enters again
    // on worker 1 before exiting on worker 0 (the second enter should be ignored).
    const customEvents = [
      // Worker 1 events arrive first in the array but have later timestamps
      { name: "SpanEnterEvent", timestamp: 2000, fields: { worker_id: 1, span_id: 2, parent_span_id: null, span_name: "b" } },
      { name: "SpanExitEvent", timestamp: 2500, fields: { worker_id: 1, span_id: 2, span_name: "b" } },
      // Worker 0 events arrive second but have earlier timestamps
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "a" } },
      // Duplicate enter on worker 1 before exit on worker 0 (should be ignored)
      { name: "SpanEnterEvent", timestamp: 1200, fields: { worker_id: 1, span_id: 1, parent_span_id: null, span_name: "a" } },
      { name: "SpanExitEvent", timestamp: 1500, fields: { worker_id: 0, span_id: 1, span_name: "a" } },
      { name: "SpanCloseEvent", timestamp: 3000, fields: { span_id: 1 } },
      { name: "SpanCloseEvent", timestamp: 3001, fields: { span_id: 2 } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    expect(allSpans.length, `Expected 2 spans, got ${allSpans.length}`).toBe(2);
    const spanA = allSpans.find((s: any) => s.spanName === "a");
    const spanB = allSpans.find((s: any) => s.spanName === "b");
    expect(spanA && spanB, "Expected spans 'a' and 'b'").toBeTruthy();
    // Span A: entered at 1000, exited at 1500 (duplicate enter at 1200 ignored)
    expect(
      spanA.segments.length,
      `Expected 1 segment for 'a', got ${spanA.segments.length}`,
    ).toBe(1);
    expect(
      spanA.segments[0].start,
      `Expected segment start=1000, got ${spanA.segments[0].start}`,
    ).toBe(1000);
    expect(
      spanA.segments[0].end,
      `Expected segment end=1500, got ${spanA.segments[0].end}`,
    ).toBe(1500);
    // Span B: entered at 2000, exited at 2500
    expect(
      spanB.segments[0].start === 2000 && spanB.segments[0].end === 2500,
      `Span B segment wrong: ${spanB.segments[0].start}-${spanB.segments[0].end}`,
    ).toBe(true);
  });

  it("handles old and new span event fields without leaking built-ins", () => {
    const oldEvents = [
      { name: "SpanEnter:test::op", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "op" } },
      { name: "SpanExit:test::op", timestamp: 2000, fields: { worker_id: 0, span_id: 1, span_name: "op" } },
      { name: "SpanCloseEvent", timestamp: 2001, fields: { span_id: 1 } },
    ];
    const oldResult = buildSpanData(oldEvents);
    expect(oldResult.allSpans).toHaveLength(1);
    expect(oldResult.allSpans[0].spanName).toBe("op");
    expect(oldResult.allSpans[0].fields.span_instance_id).toBeFalsy();
    expect(oldResult.allSpans[0].fields.tid).toBeFalsy();

    const newEvents = [
      { name: "SpanEnter:test::op", timestamp: 1000, fields: { worker_id: 0, span_id: 1, span_instance_id: 42, tid: 12345, parent_span_id: null, span_name: "op", user_field: "val" } },
      { name: "SpanExit:test::op", timestamp: 2000, fields: { worker_id: 0, span_id: 1, span_instance_id: 42, tid: 12345, span_name: "op", user_field: "val" } },
      { name: "SpanCloseEvent", timestamp: 2001, fields: { span_id: 1 } },
    ];
    const newResult = buildSpanData(newEvents);
    expect(newResult.allSpans).toHaveLength(1);
    expect(newResult.allSpans[0].spanName).toBe("op");
    expect(newResult.allSpans[0].fields.span_instance_id).toBeFalsy();
    expect(newResult.allSpans[0].fields.tid).toBeFalsy();
    expect(newResult.allSpans[0].fields.user_field).toBe("val");
  });

  it("derived-struct span schema (SpanEnter__/SpanExit__) parsed correctly", () => {
    // Hand-written `#[derive(TraceEvent)]` span structs cannot put ':' in their
    // wire name (it's a Rust identifier), so they use the "SpanEnter__"/
    // "SpanExit__" prefix instead. The viewer must classify these as spans too.
    const customEvents = [
      { name: "SpanEnter__DemoOp", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "request", operation: "GET /widgets", detail: "request #0" } },
      { name: "SpanEnter__DemoOp", timestamp: 1100, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "db_query", operation: "SELECT widgets", detail: "query for request #0" } },
      { name: "SpanExit__DemoOp", timestamp: 1200, fields: { worker_id: 0, span_id: 2, span_name: "db_query" } },
      { name: "SpanExit__DemoOp", timestamp: 1300, fields: { worker_id: 0, span_id: 1, span_name: "request" } },
    ];
    const { allSpans, maxDepth } = buildSpanData(customEvents);
    expect(allSpans.length, `Expected 2 spans, got ${allSpans.length}`).toBe(2);
    const req = allSpans.find((s: any) => s.spanName === "request");
    const q = allSpans.find((s: any) => s.spanName === "db_query");
    expect(req && q, "Missing request or db_query span").toBeTruthy();
    // Both an interned (operation) and an inline (detail) user field should
    // surface; base fields should not.
    expect(
      req.fields.operation,
      `Expected operation='GET /widgets', got '${req.fields.operation}'`,
    ).toBe("GET /widgets");
    expect(
      req.fields.detail,
      `Expected detail='request #0', got '${req.fields.detail}'`,
    ).toBe("request #0");
    expect(req.fields.span_name, "span_name should not be in user fields").toBeFalsy();
    // Nesting via parent_span_id must be reconstructed.
    expect(q.parentSpanId, `Expected db_query parent '1', got '${q.parentSpanId}'`).toBe("1");
    expect(maxDepth, `Expected maxDepth=1, got ${maxDepth}`).toBe(1);
  });

  it("cross-worker enter/exit: segment carries the enter worker", () => {
    // Regression: a long-lived span entered on one worker and exited on a
    // different worker (its task migrated between enter and exit). The segment's
    // `start` is the ENTER timestamp, so the segment must carry the ENTER
    // worker — otherwise the viewer's span->task lookup (find the poll on
    // segment.workerId that overlaps segment.start) searches the wrong worker's
    // poll timeline at the enter time, finds nothing, and fails to select a
    // task. Single enter/exit pair, workers differ.
    const customEvents = [
      { name: "SpanEnter:app::req:req.rs:1", timestamp: 1000, fields: { worker_id: 3, span_id: 1, parent_span_id: null, span_name: "handle_request" } },
      { name: "SpanExit:app::req:req.rs:1", timestamp: 5000, fields: { worker_id: 7, span_id: 1, span_name: "handle_request" } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    expect(allSpans.length, `Expected 1 span, got ${allSpans.length}`).toBe(1);
    const seg = allSpans[0].segments[0];
    expect(
      seg.start === 1000 && seg.end === 5000,
      `Segment timing wrong: ${seg.start}-${seg.end}`,
    ).toBe(true);
    // The segment start is the enter time (1000, on worker 3), so the segment
    // must be attributed to worker 3, not the exit worker 7.
    expect(seg.workerId, `Expected segment workerId=3 (enter worker), got ${seg.workerId}`).toBe(3);
  });

  it("reconstructs active/idle segments from the owning task's polls", () => {
    // A long-lived request span: single SpanEnter/SpanExit pair [1000, 9000],
    // but its owning task (id 42) was only polled twice in that window. The
    // enter (worker 0, t=1000) falls inside poll [900,1100] which identifies
    // task 42. Reconstruction should replace the one coarse segment with the
    // two real on-CPU polls and expose the idle gap between them.
    const workerSpans = {
      0: { polls: [
        { start: 900, end: 1100, taskId: 42 },   // covers the enter -> resolves task 42
        { start: 8800, end: 8900, taskId: 42 },
      ] },
      1: { polls: [
        { start: 4000, end: 4100, taskId: 42 },   // task migrated to worker 1
      ] },
    };
    const customEvents = [
      { name: "SpanEnter:app::req:req.rs:1", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "GET /jobs/next" } },
      { name: "SpanExit:app::req:req.rs:1", timestamp: 9000, fields: { worker_id: 1, span_id: 1, span_name: "GET /jobs/next" } },
    ];
    const { allSpans } = buildSpanData(customEvents, workerSpans);
    expect(allSpans.length, `Expected 1 span, got ${allSpans.length}`).toBe(1);
    const s = allSpans[0];
    // Span lifetime unchanged (the on-wire enter/exit).
    expect(s.start === 1000 && s.end === 9000, `Span window wrong: ${s.start}-${s.end}`).toBe(true);
    // Owning task resolved and stored on the span.
    expect(s.taskId, `Expected taskId=42, got ${s.taskId}`).toBe(42);
    // Three polls of task 42 fall in [1000,9000]: [1000,1100], [4000,4100], [8800,8900].
    expect(s.segments.length, `Expected 3 reconstructed segments, got ${s.segments.length}`).toBe(3);
    expect(
      s.segments[0].start === 1000 && s.segments[0].end === 1100,
      `seg0 wrong: ${JSON.stringify(s.segments[0])}`,
    ).toBe(true);
    expect(
      s.segments[1].workerId,
      `seg1 should be on worker 1 (migrated), got ${s.segments[1].workerId}`,
    ).toBe(1);
    // activeNs = 100 + 100 + 100 = 300, far below the 8000 wall-clock lifetime.
    expect(s.activeNs, `Expected activeNs=300, got ${s.activeNs}`).toBe(300);
    const idle = (s.end - s.start) - s.activeNs;
    expect(idle, `Expected idle=7700, got ${idle}`).toBe(7700);
  });

  it("prefers the namespaced task ID and preserves an application task_id", () => {
    const workerSpans = {
      // If the reader incorrectly falls back through worker 0, it will pick 99.
      0: { polls: [{ start: 900, end: 1100, taskId: 99 }] },
      1: { polls: [
        { start: 900, end: 1100, taskId: 42 },
        { start: 4000, end: 4100, taskId: 42 },
      ] },
    };
    const customEvents = [
      { name: "SpanEnter:app::req:req.rs:1", timestamp: 1000, fields: { worker_id: 0, "dial9.tokio.task_id": 42, task_id: "application-task", span_id: 1, parent_span_id: null, span_name: "request" } },
      { name: "SpanExit:app::req:req.rs:1", timestamp: 5000, fields: { worker_id: 0, "dial9.tokio.task_id": 42, task_id: "application-task", span_id: 1, span_name: "request" } },
    ];

    const { allSpans } = buildSpanData(customEvents, workerSpans);
    const span = allSpans[0];
    expect(span.taskId).toBe(42);
    expect(span.segments).toEqual([
      { start: 1000, end: 1100, workerId: 1 },
      { start: 4000, end: 4100, workerId: 1 },
    ]);
    expect(span.fields.task_id).toBe("application-task");
  });

  it("treats task_id as application data on legacy worker spans", () => {
    const workerSpans = {
      0: { polls: [{ start: 900, end: 1600, taskId: 99 }] },
    };
    const customEvents = [
      { name: "SpanEnter:app::req:req.rs:1", timestamp: 1000, fields: { worker_id: 0, task_id: "42", span_id: 1, parent_span_id: null, span_name: "request" } },
      { name: "SpanExit:app::req:req.rs:1", timestamp: 1500, fields: { worker_id: 0, task_id: "42", span_id: 1, span_name: "request" } },
    ];

    const { allSpans } = buildSpanData(customEvents, workerSpans);
    expect(allSpans[0].taskId).toBe(99);
    expect(allSpans[0].fields.task_id).toBe("42");
  });

  it("without workerSpans keeps raw segments (backwards compatible)", () => {
    // Without workerSpans, behavior is unchanged: raw on-wire segment, no taskId.
    const customEvents = [
      { name: "SpanEnter:app::req:req.rs:1", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "req" } },
      { name: "SpanExit:app::req:req.rs:1", timestamp: 9000, fields: { worker_id: 0, span_id: 1, span_name: "req" } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    expect(allSpans.length, `Expected 1 span, got ${allSpans.length}`).toBe(1);
    const s = allSpans[0];
    expect(s.segments.length, `Expected 1 raw segment, got ${s.segments.length}`).toBe(1);
    expect(s.activeNs, `Expected activeNs=8000 (raw), got ${s.activeNs}`).toBe(8000);
    expect(s.taskId, `Expected taskId=null without workerSpans, got ${s.taskId}`).toBeNull();
  });

  it("does not borrow a recycled task-id's later polls", () => {
    // Task id 7 is recycled: instance A polled at [1000,1100], then a LATER
    // instance polled at [50000,50100]. A span owned by instance A (enter at
    // 1000, exit at 1200) must only pick up A's poll, not the recycled
    // instance's — the `p.start > seg.end` break in reconstructSpanSegments
    // guards this.
    const workerSpans = {
      0: { polls: [
        { start: 900, end: 1100, taskId: 7 },   // instance A, covers enter
        { start: 50000, end: 50100, taskId: 7 }, // recycled instance, far later
      ] },
    };
    const customEvents = [
      { name: "SpanEnter:app::f:f:1", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "f" } },
      { name: "SpanExit:app::f:f:1", timestamp: 1200, fields: { worker_id: 0, span_id: 1, span_name: "f" } },
    ];
    const { allSpans } = buildSpanData(customEvents, workerSpans);
    const s = allSpans[0];
    expect(s.taskId, `Expected taskId=7, got ${s.taskId}`).toBe(7);
    expect(
      s.segments.length,
      `Expected 1 segment (recycled poll excluded), got ${s.segments.length}`,
    ).toBe(1);
    expect(s.segments[0].end, `Expected segment clamped to 1100, got ${s.segments[0].end}`).toBe(1100);
    expect(s.activeNs, `Expected activeNs=100, got ${s.activeNs}`).toBe(100);
  });
});

// ── reconstructSpanSegments (unit) ──

describe("reconstructSpanSegments", () => {
  it("carves a coarse segment into per-poll active segments", () => {
    // One coarse segment [0,1000] intersected with a task polled twice
    // ([100,200] on w0, [500,600] on w1). Result: two active segments with an
    // idle gap between; workers come from the polls, not the raw segment.
    const raw = [{ start: 0, end: 1000, workerId: 9 }];
    const taskPolls = [
      { start: 100, end: 200, workerId: 0 },
      { start: 500, end: 600, workerId: 1 },
    ];
    const segs = reconstructSpanSegments(raw, taskPolls);
    expect(segs.length, `Expected 2 segments, got ${segs.length}`).toBe(2);
    expect(
      segs[0].start === 100 && segs[0].end === 200 && segs[0].workerId === 0,
      `seg0 wrong: ${JSON.stringify(segs[0])}`,
    ).toBe(true);
    expect(
      segs[1].start === 500 && segs[1].end === 600 && segs[1].workerId === 1,
      `seg1 wrong: ${JSON.stringify(segs[1])}`,
    ).toBe(true);
  });

  it("clamps overlapping polls to the segment window", () => {
    // Polls extending past the segment on both ends are clamped to [start,end].
    const raw = [{ start: 300, end: 700, workerId: 0 }];
    const taskPolls = [
      { start: 100, end: 400, workerId: 0 }, // overlaps left edge
      { start: 600, end: 900, workerId: 0 }, // overlaps right edge
    ];
    const segs = reconstructSpanSegments(raw, taskPolls);
    expect(segs.length, `Expected 2 segments, got ${segs.length}`).toBe(2);
    expect(
      segs[0].start === 300 && segs[0].end === 400,
      `left clamp wrong: ${JSON.stringify(segs[0])}`,
    ).toBe(true);
    expect(
      segs[1].start === 600 && segs[1].end === 700,
      `right clamp wrong: ${JSON.stringify(segs[1])}`,
    ).toBe(true);
  });

  it("falls back to raw segments when no poll overlaps", () => {
    // Span window falls entirely between the task's polls -> no intersection.
    // Rather than drop the span, keep the raw segments so it still renders.
    const raw = [{ start: 300, end: 400, workerId: 5 }];
    const taskPolls = [
      { start: 0, end: 100, workerId: 0 },
      { start: 700, end: 800, workerId: 0 },
    ];
    const segs = reconstructSpanSegments(raw, taskPolls);
    expect(segs, "Expected raw segments returned by reference on no overlap").toBe(raw);
  });

  it("is identity when task has no polls", () => {
    const raw = [{ start: 0, end: 10, workerId: 0 }];
    expect(reconstructSpanSegments(raw, undefined), "undefined polls should return raw").toBe(raw);
    expect(reconstructSpanSegments(raw, []), "empty polls should return raw").toBe(raw);
  });

  it("intersects each raw segment independently (gap polls excluded)", () => {
    // A span already carrying two per-poll raw segments (e.g. re-entered).
    // Each is intersected independently; a poll landing in the gap BETWEEN the
    // two raw segments must NOT be attributed to the span.
    const raw = [
      { start: 100, end: 200, workerId: 0 },
      { start: 500, end: 600, workerId: 0 },
    ];
    const taskPolls = [
      { start: 120, end: 180, workerId: 0 }, // inside raw[0]
      { start: 300, end: 400, workerId: 0 }, // in the gap — must be excluded
      { start: 520, end: 640, workerId: 1 }, // overlaps raw[1], clamped to 600
    ];
    const segs = reconstructSpanSegments(raw, taskPolls);
    expect(
      segs.length,
      `Expected 2 segments (gap poll excluded), got ${segs.length}: ${JSON.stringify(segs)}`,
    ).toBe(2);
    expect(
      segs[0].start === 120 && segs[0].end === 180,
      `seg0 wrong: ${JSON.stringify(segs[0])}`,
    ).toBe(true);
    expect(
      segs[1].start === 520 && segs[1].end === 600 && segs[1].workerId === 1,
      `seg1 wrong: ${JSON.stringify(segs[1])}`,
    ).toBe(true);
  });
});

// ── resolveSpanTask (unit) ──

describe("resolveSpanTask", () => {
  it("binary-searches the covering poll; gaps and unknown workers -> null", () => {
    // Covering poll found among many; and a timestamp in an inter-poll gap
    // resolves to null (not the nearest poll).
    const workerSpans = {
      2: { polls: [
        { start: 0, end: 100, taskId: 11 },
        { start: 200, end: 300, taskId: 22 },
        { start: 400, end: 500, taskId: 33 },
      ] },
    };
    expect(resolveSpanTask({ start: 250, workerId: 2 }, workerSpans), "Expected task 22 for ts=250").toBe(22);
    expect(resolveSpanTask({ start: 400, workerId: 2 }, workerSpans), "Expected task 33 at poll start edge").toBe(33);
    expect(resolveSpanTask({ start: 150, workerId: 2 }, workerSpans), "Gap timestamp should resolve to null").toBeNull();
    expect(resolveSpanTask({ start: 250, workerId: 9 }, workerSpans), "Unknown worker should resolve to null").toBeNull();
  });

  it("treats a covering poll with taskId 0 as null", () => {
    // A poll covers the timestamp but has taskId 0 (block-in-place stub) ->
    // treated as "no task", not task 0.
    const workerSpans = { 0: { polls: [{ start: 0, end: 100, taskId: 0 }] } };
    expect(resolveSpanTask({ start: 50, workerId: 0 }, workerSpans), "taskId 0 should resolve to null").toBeNull();
  });
});

// ── span pane layout ──

describe("span pane layout", () => {
  it("collectDescendants returns id plus all descendants (cycle-safe)", () => {
    // Same tree: r1 → {c1 → g1, c2}, r2 (no children)
    const childrenByParent = new Map<string | null, string[]>([
      [null, ["1", "5"]],
      ["1", ["2", "4"]],
      ["2", ["3"]],
    ]);
    const d1 = collectDescendants(["1"], childrenByParent);
    // Should include 1, 2, 3, 4 (but not 5)
    expect(
      d1.has("1") && d1.has("2") && d1.has("3") && d1.has("4"),
      `Expected {"1","2","3","4"} in descendants of "1", got ${[...d1]}`,
    ).toBe(true);
    expect(d1.has("5"), "r2 should not be in descendants of r1").toBe(false);
    expect(d1.size, `Expected size 4, got ${d1.size}`).toBe(4);

    const d5 = collectDescendants(["5"], childrenByParent);
    expect(
      d5.size === 1 && d5.has("5"),
      `Expected only {"5"}, got ${[...d5]}`,
    ).toBe(true);

    // Guard against cycles (children references ancestor)
    const cyclic = new Map<string | null, string[]>([
      ["1", ["2"]],
      ["2", ["1"]], // cycle
    ]);
    const dc = collectDescendants(["1"], cyclic);
    expect(dc.has("1") && dc.has("2"), "Cycle should still produce set").toBe(true);
  });

  it("selectSpanRenderSet returns only root-like spans when focus is null", () => {
    // When no focus, return only spans whose parent is null OR whose parent is absent
    const spans = [
      { spanId: "1", parentSpanId: null, spanName: "r1" },
      { spanId: "2", parentSpanId: "1", spanName: "c1" },
      { spanId: "3", parentSpanId: "99", spanName: "orphan" }, // parent not in set
      { spanId: "5", parentSpanId: null, spanName: "r2" },
    ];
    const childrenByParent = new Map<string | null, string[]>([
      [null, ["1", "5"]],
      ["1", ["2"]],
    ]);
    const result = selectSpanRenderSet({
      allSpans: spans,
      focusedSpanId: null,
      childrenByParent,
    });
    const ids = new Set(result.map((s: any) => s.spanId));
    expect(
      ids.has("1") && ids.has("5") && ids.has("3"),
      `Expected {"1","3","5"}, got ${[...ids]}`,
    ).toBe(true);
    expect(ids.has("2"), "Child span 2 should not be rendered in root view").toBe(false);
  });

  it("selectSpanRenderSet returns focused span + descendants", () => {
    const spans = [
      { spanId: "1", parentSpanId: null, spanName: "r1" },
      { spanId: "2", parentSpanId: "1", spanName: "c1" },
      { spanId: "3", parentSpanId: "2", spanName: "g1" },
      { spanId: "4", parentSpanId: "1", spanName: "c2" },
      { spanId: "5", parentSpanId: null, spanName: "r2" },
    ];
    const childrenByParent = new Map<string | null, string[]>([
      [null, ["1", "5"]],
      ["1", ["2", "4"]],
      ["2", ["3"]],
    ]);
    const result = selectSpanRenderSet({
      allSpans: spans,
      focusedSpanId: "1",
      childrenByParent,
    });
    const ids = new Set(result.map((s: any) => s.spanId));
    // Focus on 1: should include 1 itself and all descendants (2, 3, 4). Not 5.
    expect(
      ids.has("1") && ids.has("2") && ids.has("3") && ids.has("4"),
      `Expected focused set to include {"1","2","3","4"}, got ${[...ids]}`,
    ).toBe(true);
    expect(ids.has("5"), "Sibling root 5 should not be in focused set").toBe(false);
  });

  it("computeSpanLayout places longer spans higher (smaller y)", () => {
    // Three spans with very different durations. Panel: 100 px wide, 60 px tall.
    // Longest → smallest y (near top). Shortest → largest y (near bottom).
    const spans = [
      { spanId: 1, start: 0, end: 100, spanName: "tiny", segments: [], activeNs: 100 },
      { spanId: 2, start: 10, end: 1010, spanName: "medium", segments: [], activeNs: 1000 },
      { spanId: 3, start: 20, end: 10020, spanName: "huge", segments: [], activeNs: 10000 },
    ];
    const layout = computeSpanLayout({
      spans,
      viewStart: 0,
      viewEnd: 10020,
      drawW: 1000,
      panelH: 60,
      clusterXPx: 2,
      barH: 4,
    });
    expect(layout && layout.buckets, "computeSpanLayout must return {buckets}").toBeTruthy();
    // Should produce one bucket per span (no clustering at this wide view).
    expect(
      layout.buckets.length,
      `Expected 3 buckets, got ${layout.buckets.length}`,
    ).toBe(3);
    // Find buckets by representative spanId.
    const byId = new Map<unknown, any>();
    for (const b of layout.buckets) byId.set(b.representative.spanId, b);
    const yTiny = byId.get(1).y;
    const yMed = byId.get(2).y;
    const yHuge = byId.get(3).y;
    // Larger duration → smaller y (higher on screen)
    expect(
      yHuge < yMed && yMed < yTiny,
      `Expected y(huge) < y(medium) < y(tiny), got ${yHuge} < ${yMed} < ${yTiny}`,
    ).toBe(true);
    // All y within panel
    const outside = layout.buckets.filter((b: any) => b.y < 0 || b.y + b.h > 60 + 1);
    expect(
      outside.map((b: any) => `Bucket y=${b.y}, h=${b.h} outside panel 60`),
    ).toEqual([]);
  });

  it("computeSpanLayout clusters overlapping spans into single bucket", () => {
    // Many spans with identical duration piled at the same x — should cluster.
    const spans: any[] = [];
    for (let i = 0; i < 10; i++) {
      spans.push({ spanId: i + 1, start: 100, end: 200, spanName: "same", segments: [], activeNs: 100 });
    }
    // Add one outlier with different duration (far away on y axis)
    spans.push({ spanId: 100, start: 100, end: 10000, spanName: "outlier", segments: [], activeNs: 9900 });
    const layout = computeSpanLayout({
      spans,
      viewStart: 0,
      viewEnd: 10000,
      drawW: 500,
      panelH: 60,
      clusterXPx: 4,
      barH: 4,
    });
    // Expect the 10 identical spans to cluster into 1 bucket, plus the outlier in its own bucket.
    expect(
      layout.buckets.length,
      `Expected 2 buckets (cluster + outlier), got ${layout.buckets.length}`,
    ).toBe(2);
    const cluster = layout.buckets.find((b: any) => b.spans.length > 1);
    expect(cluster, "Expected a cluster bucket").toBeTruthy();
    expect(cluster.spans.length, `Expected cluster size 10, got ${cluster.spans.length}`).toBe(10);
    // representative should be one of the clustered spans
    expect(
      cluster.spans.includes(cluster.representative),
      "Representative should be a member of cluster.spans",
    ).toBe(true);
  });

  it("computeSpanLayout picks representative from cluster members", () => {
    // Several spans at the same position. Representative should be the longest.
    // All have the same start/end (same duration → same y), so they cluster.
    // We differentiate by activeNs to verify representative selection uses total duration.
    const spans = [
      { spanId: 1, start: 100, end: 200, spanName: "a", segments: [], activeNs: 50 },
      { spanId: 2, start: 100, end: 200, spanName: "b", segments: [], activeNs: 100 },
      { spanId: 3, start: 100, end: 200, spanName: "c", segments: [], activeNs: 80 },
    ];
    const layout = computeSpanLayout({
      spans,
      viewStart: 0,
      viewEnd: 500,
      drawW: 10,
      panelH: 60,
      clusterXPx: 100,
      barH: 4,
    });
    // Same duration → same y → same cell → one cluster
    const clustered = layout.buckets.find((b: any) => b.spans.length === 3);
    expect(
      clustered,
      `Expected single 3-span cluster, got ${JSON.stringify(layout.buckets.map((b: any) => b.spans.length))}`,
    ).toBeTruthy();
    // All have same duration, so any is valid as representative (first encountered wins tie)
    // The key property: representative is a member of the cluster
    expect(
      clustered.spans.includes(clustered.representative),
      "Representative should be a member of cluster.spans",
    ).toBe(true);
  });
});

// ── Block-in-place active-span suppression ──

describe("block-in-place active-span suppression", () => {
  it("active span crossing block-in-place gap is suppressed; clean span preserved", () => {
    // Synthetic events: worker 0 unparks on tid=42, then parks on tid=99
    // (a block_in_place handoff). The active span [10, 50) crosses the gap
    // and must be discarded. A subsequent normal active span [60, 70) on
    // tid=99 should be preserved.
    const syntheticEvents = [
      { eventType: EVENT_TYPES["WorkerUnpark"], timestamp: 10, workerId: 0, tid: 42, cpuTime: 100, schedWait: 0, localQueue: 0, globalQueue: 0, taskId: 0, spawnLocId: null, spawnLoc: null },
      { eventType: EVENT_TYPES["WorkerPark"], timestamp: 50, workerId: 0, tid: 99, cpuTime: 500, localQueue: 0, globalQueue: 0, schedWait: 0, taskId: 0, spawnLocId: null, spawnLoc: null },
      { eventType: EVENT_TYPES["WorkerUnpark"], timestamp: 60, workerId: 0, tid: 99, cpuTime: 600, schedWait: 0, localQueue: 0, globalQueue: 0, taskId: 0, spawnLocId: null, spawnLoc: null },
      { eventType: EVENT_TYPES["WorkerPark"], timestamp: 70, workerId: 0, tid: 99, cpuTime: 700, localQueue: 0, globalQueue: 0, schedWait: 0, taskId: 0, spawnLocId: null, spawnLoc: null },
    ];
    const gaps = [{ workerId: 0, fromTid: 42, toTid: 99, startNs: 10, endNs: 50 }];
    const result = buildWorkerSpans(syntheticEvents, [0], 100, gaps);
    const actives = result.workerSpans[0].actives;
    // The first active [10,50) crosses the gap → suppressed.
    // The second active [60,70) is clean → preserved.
    expect(
      actives.length,
      `Expected 1 active span (gap-crossing suppressed), got ${actives.length}: ${JSON.stringify(actives)}`,
    ).toBe(1);
    expect(
      actives[0].start === 60 && actives[0].end === 70,
      `Expected active [60,70), got [${actives[0].start},${actives[0].end})`,
    ).toBe(true);
  });
});

// ── computePollWakes ──

// Reference O(P^2) implementation — the original viewer loop, kept here to
// prove the binary-search version produces identical results.
function pollWakesBruteForce(
  polls: { start: number; end: number }[],
  wakes: { timestamp: number; wakerTaskId: number }[],
): Array<{ wake: { wakerTaskId: number }; effectiveWake: number } | null> {
  const out: Array<{ wake: { wakerTaskId: number }; effectiveWake: number } | null> = [];
  for (let pi = 0; pi < polls.length; pi++) {
    const s = polls[pi]!;
    let best: { wake: { wakerTaskId: number }; effectiveWake: number } | null = null;
    if (wakes.length) {
      let lo = 0,
        hi = wakes.length - 1,
        bi = -1;
      while (lo <= hi) {
        const mid = (lo + hi) >> 1;
        if (wakes[mid]!.timestamp <= s.start) {
          bi = mid;
          lo = mid + 1;
        } else hi = mid - 1;
      }
      if (bi >= 0) {
        const w = wakes[bi]!;
        let effectiveWake = w.timestamp;
        for (let j = 0; j < pi; j++) {
          if (w.timestamp >= polls[j]!.start && w.timestamp <= polls[j]!.end) {
            effectiveWake = polls[j]!.end;
            break;
          }
        }
        const delay = s.start - effectiveWake;
        if (delay >= 0 && delay < 1e9) best = { wake: w, effectiveWake };
      }
    }
    out.push(best);
  }
  return out;
}

describe("computePollWakes", () => {
  it("binary-search computePollWakes matches O(P^2) reference (400 polls, 600 wakes)", () => {
    // Deterministic pseudo-random non-overlapping polls + scattered wakes.
    const polls: { start: number; end: number }[] = [];
    let t = 0;
    let seed = 12345;
    const rnd = () => (seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff;
    for (let i = 0; i < 400; i++) {
      const gap = Math.floor(rnd() * 50);
      const dur = 1 + Math.floor(rnd() * 80);
      const start = t + gap;
      polls.push({ start, end: start + dur });
      t = start + dur;
    }
    const wakes: { timestamp: number; wakerTaskId: number }[] = [];
    for (let i = 0; i < 600; i++) {
      wakes.push({ timestamp: Math.floor(rnd() * t), wakerTaskId: i });
    }
    // Deliberately seed wakes EXACTLY on poll boundaries (poll.start, which for
    // a zero-gap poll equals the previous poll's end). This is the case where a
    // wake is contained by two adjacent polls and where the binary-search and
    // O(P^2) versions can disagree on which poll "owns" it — so the comparison
    // below actually exercises the first-match tie-break, not just interiors.
    for (let i = 0; i < polls.length; i += 7) {
      wakes.push({ timestamp: polls[i]!.start, wakerTaskId: 1000 + i });
      wakes.push({ timestamp: polls[i]!.end, wakerTaskId: 2000 + i });
    }
    wakes.sort((a, b) => a.timestamp - b.timestamp);

    const fast = computePollWakes(polls, wakes);
    const slow = pollWakesBruteForce(polls, wakes);
    expect(
      fast.length,
      `pollWakes: length mismatch ${fast.length} vs ${slow.length}`,
    ).toBe(slow.length);
    const mismatches: string[] = [];
    for (let i = 0; i < slow.length; i++) {
      const a = fast[i],
        b = slow[i];
      if ((a == null) !== (b == null)) mismatches.push(`pollWakes[${i}]: null mismatch`);
      if (a && b) {
        if (a.effectiveWake !== b.effectiveWake)
          mismatches.push(
            `pollWakes[${i}]: effectiveWake ${a.effectiveWake} vs ${b.effectiveWake}`,
          );
        if (a.wake.wakerTaskId !== b.wake.wakerTaskId)
          mismatches.push(`pollWakes[${i}]: wake mismatch`);
      }
    }
    expect(mismatches).toEqual([]);
  });

  it("no wakes yields all-null result", () => {
    const polls = [
      { start: 0, end: 10 },
      { start: 20, end: 30 },
    ];
    const out = computePollWakes(polls, []);
    expect(
      out.length === 2 && out[0] === null && out[1] === null,
      "pollWakes: empty wakes should yield all-null",
    ).toBe(true);
  });

  it("wake landing mid-earlier-poll bumps effectiveWake to that poll's end", () => {
    // Wake at t=5 lands inside poll[0] [0,10]; poll[1] starts at 20.
    // effectiveWake for poll[1] must bump to poll[0].end (10), not 5.
    const polls = [
      { start: 0, end: 10 },
      { start: 20, end: 30 },
    ];
    const wakes = [{ timestamp: 5, wakerTaskId: 99 }];
    const out = computePollWakes(polls, wakes);
    // poll[0]: wake at 5 <= start 0? no — rightmost wake <= 0 is none → null.
    expect(out[0], "pollWakes: poll[0] should have no qualifying wake").toBeNull();
    expect(
      out[1] && out[1].effectiveWake === 10,
      `pollWakes: poll[1] effectiveWake should bump to 10, got ${out[1] && out[1].effectiveWake}`,
    ).toBeTruthy();
  });

  it("wake on a shared poll boundary matches first-match (lowest-index) semantics", () => {
    // A wake landing on a shared poll boundary (poll0.end == poll1.start == t)
    // is contained by BOTH adjacent polls. The original O(P^2) loop took the
    // FIRST (lowest-index) match, so effectiveWake = poll0.end == t (no bump).
    // The binary search finds the rightmost poll with start <= t (poll1), so it
    // must walk left to poll0 to stay faithful. Without that walk it would
    // wrongly report poll1.end.
    const polls = [
      { start: 0, end: 10 },
      { start: 10, end: 20 },
      { start: 30, end: 40 },
    ];
    const wakes = [{ timestamp: 10, wakerTaskId: 7 }];
    const out = computePollWakes(polls, wakes);
    expect(
      out[2] && out[2].effectiveWake === 10,
      `pollWakes: shared-boundary effectiveWake should be 10 (first match), got ${out[2] && out[2].effectiveWake}`,
    ).toBeTruthy();

    // Zero-width poll chain all touching t=10: lowest-index match is poll0.
    const chain = [
      { start: 0, end: 10 },
      { start: 10, end: 10 },
      { start: 10, end: 20 },
      { start: 30, end: 40 },
    ];
    const cout = computePollWakes(chain, wakes);
    expect(
      cout[3] && cout[3].effectiveWake === 10,
      `pollWakes: boundary-chain effectiveWake should be 10, got ${cout[3] && cout[3].effectiveWake}`,
    ).toBeTruthy();
  });
});

// ── pixelDownsampleSpans ──

const _dur = (s: { start: number; end: number }) => s.end - s.start;
function ffvByStart(spans: { start: number; end: number }[], vs: number): number {
  let lo = 0,
    hi = spans.length - 1;
  while (lo <= hi) {
    const m = (lo + hi) >> 1;
    if (spans[m]!.end < vs) lo = m + 1;
    else hi = m - 1;
  }
  return lo;
}

describe("pixelDownsampleSpans", () => {
  it("bounds output by pixels (100k spans over 200px)", () => {
    // 100k spans over a 200px lane → at most 200 representatives.
    const spans: { start: number; end: number }[] = [];
    for (let i = 0; i < 100000; i++) spans.push({ start: i, end: i + 1 });
    const reps = pixelDownsampleSpans(spans, 0, 0, 100000, 200, _dur);
    expect(reps.length, `downsample: expected <=200 reps, got ${reps.length}`).toBeLessThanOrEqual(200);
    expect(reps.length, "downsample: expected some reps").toBeGreaterThanOrEqual(1);
  });

  it("longest span wins its pixel column", () => {
    // Three spans in the same pixel column; the longest must be the rep.
    // viewDur=1000 over pw=10 → 100ns per pixel. All three start in [0,100).
    const spans = [
      { start: 0, end: 5, id: "a" },
      { start: 10, end: 90, id: "b" }, // longest
      { start: 20, end: 25, id: "c" },
    ];
    const reps = pixelDownsampleSpans(spans, 0, 0, 1000, 10, _dur);
    expect(reps.length, `downsample: expected 1 rep in column, got ${reps.length}`).toBe(1);
    expect(reps[0].id, `downsample: expected longest 'b', got '${reps[0].id}'`).toBe("b");
  });

  it("sparse spans pass through unchanged", () => {
    // Spans already spread > 1px apart: all survive, order preserved.
    const spans = [
      { start: 0, end: 10 },
      { start: 500, end: 510 },
      { start: 999, end: 1000 },
    ];
    const reps = pixelDownsampleSpans(spans, 0, 0, 1000, 1000, _dur);
    expect(reps.length, `downsample: expected 3 reps when sparse, got ${reps.length}`).toBe(3);
    expect(
      reps[0].start === 0 && reps[2].start === 999,
      "downsample: order/identity not preserved",
    ).toBe(true);
  });

  it("respects startIdx and breaks past viewEnd", () => {
    // startIdx skips earlier spans; iteration breaks past viewEnd.
    const spans: { start: number; end: number }[] = [];
    for (let i = 0; i < 1000; i++) spans.push({ start: i * 10, end: i * 10 + 5 });
    // view [2000, 3000]; binary-search start, downsample over wide pw so no merging.
    const startIdx = ffvByStart(spans, 2000);
    const reps = pixelDownsampleSpans(spans, startIdx, 2000, 3000, 100000, _dur);
    const offenders: string[] = [];
    for (const r of reps) {
      if (r.start > 3000) offenders.push(`downsample: rep past viewEnd (${r.start})`);
      if (r.end < 2000) offenders.push(`downsample: rep before viewStart (${r.end})`);
    }
    expect(offenders).toEqual([]);
    expect(reps.length, "downsample: expected reps in window").toBeGreaterThanOrEqual(1);
  });

  it("empty / degenerate inputs yield no reps", () => {
    expect(pixelDownsampleSpans([], 0, 0, 1000, 100, _dur).length, "downsample: empty in -> empty out").toBe(0);
    expect(
      pixelDownsampleSpans([{ start: 0, end: 1 }], 0, 0, 0, 100, _dur).length,
      "downsample: zero viewDur -> empty",
    ).toBe(0);
    expect(
      pixelDownsampleSpans([{ start: 0, end: 1 }], 0, 0, 1000, 0, _dur).length,
      "downsample: zero pw -> empty",
    ).toBe(0);
  });
});

// ── pixelCoverage ──

function approx(a: number, b: number, eps?: number): boolean {
  return Math.abs(a - b) <= (eps || 1e-9);
}

describe("pixelCoverage", () => {
  it("span filling the view -> all columns fully covered", () => {
    // One span exactly filling the whole view → every column ≈ 1.
    const cov = pixelCoverage([{ start: 0, end: 100 }], 0, 0, 100, 10);
    const offenders: string[] = [];
    for (let i = 0; i < cov.length; i++)
      if (!approx(cov[i], 1)) offenders.push(`coverage: col ${i} expected ~1, got ${cov[i]}`);
    expect(offenders).toEqual([]);
  });

  it("half-covered view -> half the columns full, half empty", () => {
    // 100ns view over 10px = 10ns/px. A span covering [0,50) fills cols 0-4,
    // leaves cols 5-9 empty.
    const cov = pixelCoverage([{ start: 0, end: 50 }], 0, 0, 100, 10);
    const offenders: string[] = [];
    for (let i = 0; i < 5; i++)
      if (!approx(cov[i], 1)) offenders.push(`coverage: col ${i} expected ~1, got ${cov[i]}`);
    for (let i = 5; i < 10; i++)
      if (!approx(cov[i], 0)) offenders.push(`coverage: col ${i} expected 0, got ${cov[i]}`);
    expect(offenders).toEqual([]);
  });

  it("sparse polls produce faint (<<1) coverage, not a solid band", () => {
    // 10 tiny polls (1ns each) scattered across a 1000ns view at 10px.
    // Each column is 100ns wide; total covered time per column ≪ 1 → faint,
    // NOT solid. This is the misleading-solid-band case the helper fixes.
    const polls: { start: number; end: number }[] = [];
    for (let i = 0; i < 10; i++) polls.push({ start: i * 100, end: i * 100 + 1 });
    const cov = pixelCoverage(polls, 0, 0, 1000, 10);
    const offenders: string[] = [];
    for (let i = 0; i < cov.length; i++) {
      if (cov[i] > 0.05) offenders.push(`coverage: sparse col ${i} should be faint, got ${cov[i]}`);
      if (cov[i] <= 0) offenders.push(`coverage: sparse col ${i} should be > 0 (one poll present)`);
    }
    expect(offenders).toEqual([]);
  });

  it("span straddling columns splits coverage proportionally", () => {
    // Span [5,25) over 100ns/10px (10ns/col): col0 covered [5,10)=0.5,
    // col1 fully [10,20)=1.0, col2 [20,25)=0.5.
    const cov = pixelCoverage([{ start: 5, end: 25 }], 0, 0, 100, 10);
    expect(approx(cov[0], 0.5), `coverage: col0 expected 0.5, got ${cov[0]}`).toBe(true);
    expect(approx(cov[1], 1.0), `coverage: col1 expected 1.0, got ${cov[1]}`).toBe(true);
    expect(approx(cov[2], 0.5), `coverage: col2 expected 0.5, got ${cov[2]}`).toBe(true);
    const offenders: string[] = [];
    for (let i = 3; i < 10; i++)
      if (!approx(cov[i], 0)) offenders.push(`coverage: col ${i} expected 0`);
    expect(offenders).toEqual([]);
  });

  it("spans wider than the view clamp to <=1 per column", () => {
    // Span extends beyond both edges; only the in-view portion counts and
    // no value exceeds 1.
    const cov = pixelCoverage([{ start: -1000, end: 1000 }], 0, 0, 100, 10);
    const offenders: string[] = [];
    for (let i = 0; i < cov.length; i++) {
      if (cov[i] > 1) offenders.push(`coverage: col ${i} exceeds 1 (${cov[i]})`);
      if (!approx(cov[i], 1)) offenders.push(`coverage: col ${i} expected ~1, got ${cov[i]}`);
    }
    expect(offenders).toEqual([]);
  });

  it("degenerate inputs yield empty/zero coverage", () => {
    expect(
      pixelCoverage([], 0, 0, 100, 10).some((v: number) => v !== 0),
      "coverage: empty -> all zero",
    ).toBe(false);
    const zeroDur = pixelCoverage([{ start: 0, end: 1 }], 0, 0, 0, 10);
    expect(
      zeroDur.length !== 0 && zeroDur.some((v: number) => v !== 0),
      "coverage: zero viewDur",
    ).toBe(false);
    expect(
      pixelCoverage([{ start: 0, end: 1 }], 0, 0, 100, 0).length,
      "coverage: zero pw -> empty",
    ).toBe(0);
  });
});

// ── makeBarCoalescer ──

function collectRuns(
  pushFn: (c: any) => void,
  minWidth?: number,
): { x: number; w: number; color: string }[] {
  const runs: { x: number; w: number; color: string }[] = [];
  const c = makeBarCoalescer(
    (x: number, w: number, color: string) => runs.push({ x, w, color }),
    minWidth,
  );
  pushFn(c);
  c.flush();
  return runs;
}

describe("makeBarCoalescer", () => {
  it("sub-pixel same-color burst collapses to one rect", () => {
    // 50 same-color sub-pixel bars all landing in [10, 11) must collapse to
    // a single fillRect, not 50 of them.
    const runs = collectRuns((c) => {
      for (let i = 0; i < 50; i++) c.push(10, 10.2, "#abc");
    });
    expect(runs.length, `coalescer: expected 1 run, got ${runs.length}`).toBe(1);
    expect(runs[0]!.color, "coalescer: wrong color").toBe("#abc");
  });

  it("adjacent bars of different colors stay separate", () => {
    const runs = collectRuns((c) => {
      c.push(0, 5, "#aaa");
      c.push(5, 10, "#bbb");
      c.push(10, 15, "#aaa");
    });
    expect(runs.length, `coalescer: expected 3 runs on color change, got ${runs.length}`).toBe(3);
    expect(
      runs.map((r) => r.color).join(),
      "coalescer: colors out of order",
    ).toBe("#aaa,#bbb,#aaa");
  });

  it("same-color bars separated by a gap stay separate", () => {
    // Same color but a >1px gap between them must NOT merge.
    const runs = collectRuns((c) => {
      c.push(0, 5, "#aaa");
      c.push(20, 25, "#aaa");
    });
    expect(runs.length, `coalescer: expected 2 runs across gap, got ${runs.length}`).toBe(2);
  });

  it("min width enforced for sub-pixel runs", () => {
    const runs = collectRuns((c) => c.push(10, 10, "#aaa")); // zero-width bar
    expect(runs.length, "coalescer: expected 1 run").toBe(1);
    expect(runs[0]!.w, `coalescer: expected min width 1, got ${runs[0]!.w}`).toBe(1);
    const wide = collectRuns((c) => c.push(10, 10, "#aaa"), 3);
    expect(wide[0]!.w, `coalescer: expected min width 3, got ${wide[0]!.w}`).toBe(3);
  });

  it("no pushes emits nothing", () => {
    const runs = collectRuns(() => {});
    expect(runs.length, `coalescer: expected 0 runs, got ${runs.length}`).toBe(0);
  });

  it("contiguous same-color bars merge to their union", () => {
    // Overlapping/adjacent same-color bars extend the run; the emitted rect
    // spans the union [0, 30).
    const runs = collectRuns((c) => {
      c.push(0, 10, "#aaa");
      c.push(8, 20, "#aaa");
      c.push(20, 30, "#aaa");
    });
    expect(runs.length, `coalescer: expected 1 merged run, got ${runs.length}`).toBe(1);
    expect(
      runs[0]!.x === 0 && runs[0]!.w === 30,
      `coalescer: expected x=0 w=30, got x=${runs[0]!.x} w=${runs[0]!.w}`,
    ).toBe(true);
  });
});

// ── analyzeAllocations ──

describe("analyzeAllocations", () => {
  it("null inputs produce empty result", () => {
    const r = analyzeAllocations(null, null);
    expect(r.summary.totalAllocCount, "empty: expected 0 allocs").toBe(0);
    expect(r.perTask.size, "empty: expected empty perTask").toBe(0);
  });

  it("basic summary correct", () => {
    const allocs = [
      { timestamp: 100, tid: 10, size: 1024, addr: "0x1", callchain: ["0xa"] },
      { timestamp: 200, tid: 10, size: 2048, addr: "0x2", callchain: ["0xa"] },
    ];
    const frees = [
      { timestamp: 300, tid: 10, addr: "0x1", size: 1024, allocTimestampNs: 100 },
    ];
    const r = analyzeAllocations(allocs, frees);
    expect(r.summary.totalAllocCount, "basic: expected 2 allocs").toBe(2);
    expect(r.summary.totalFreeCount, "basic: expected 1 free").toBe(1);
    expect(r.summary.leakedCount, "basic: expected 1 leak").toBe(1);
    expect(r.summary.totalAllocBytes, "basic: expected 3072 bytes").toBe(3072);
    // With default R=524288, small allocs (s<<R) have weight ≈ R each
    // so estimatedTotalBytes ≈ 2 * 524288 (slightly above due to s/(1-exp(-s/R)) > R)
    expect(
      Math.abs(r.summary.estimatedTotalBytes - 2 * 524288),
      "basic: wrong estimatedTotalBytes",
    ).toBeLessThanOrEqual(5000);
  });

  it("per-task attribution correct", () => {
    // Worker 0 has tid=10, polling task 42 from t=50..500 and task 99 from t=600..900
    const events = [
      { eventType: 0, timestamp: 50, workerId: 0, taskId: 42 },
      { eventType: 0, timestamp: 600, workerId: 0, taskId: 99 },
    ];
    const tidToWorker = new Map([[10, 0]]);
    const allocs = [
      { timestamp: 100, tid: 10, size: 1024, addr: "0x1", callchain: ["0xa"] },
      { timestamp: 200, tid: 10, size: 2048, addr: "0x2", callchain: ["0xb"] },
      { timestamp: 700, tid: 10, size: 512, addr: "0x3", callchain: ["0xc"] },
    ];
    const frees: unknown[] = [];
    const r = analyzeAllocations(allocs, frees, { events, tidToWorker });
    expect(r.perTask.size, `perTask: expected 2 tasks, got ${r.perTask.size}`).toBe(2);
    const t42 = r.perTask.get(42);
    expect(t42, "perTask: missing task 42").toBeTruthy();
    expect(t42.count, `perTask: task 42 count=${t42.count}, expected 2`).toBe(2);
    expect(t42.sampledBytes, `perTask: task 42 sampledBytes=${t42.sampledBytes}`).toBe(3072);
    // estimatedBytes should be > sampledBytes (weight > size for small allocs)
    expect(
      t42.estimatedBytes,
      "perTask: estimatedBytes should exceed sampledBytes for small allocs",
    ).toBeGreaterThan(t42.sampledBytes);
    const t99 = r.perTask.get(99);
    expect(t99, "perTask: missing task 99").toBeTruthy();
    expect(t99.count, `perTask: task 99 count=${t99.count}, expected 1`).toBe(1);
  });

  it("non-worker tid allocations excluded from perTask", () => {
    // Alloc from tid=99 which is not in tidToWorker → should not appear in perTask
    const events = [{ eventType: 0, timestamp: 50, workerId: 0, taskId: 42 }];
    const tidToWorker = new Map([[10, 0]]);
    const allocs = [
      { timestamp: 100, tid: 99, size: 1024, addr: "0x1", callchain: ["0xa"] },
    ];
    const r = analyzeAllocations(allocs, [], { events, tidToWorker });
    expect(r.perTask.size, "nonWorkerTid: expected empty perTask").toBe(0);
  });

  it("weight(s) = s / (1 - exp(-s/R)) applied correctly", () => {
    const events = [{ eventType: 0, timestamp: 50, workerId: 0, taskId: 7 }];
    const tidToWorker = new Map([[10, 0]]);
    // Use size = sampleRateBytes so weight = s/(1-exp(-1)) ≈ 1.582*s
    const sampleRateBytes = 1000;
    const allocs = [
      { timestamp: 100, tid: 10, size: 1000, addr: "0x1", callchain: [] },
      { timestamp: 200, tid: 10, size: 1000, addr: "0x2", callchain: [] },
      { timestamp: 300, tid: 10, size: 1000, addr: "0x3", callchain: [] },
    ];
    const r = analyzeAllocations(allocs, [], { events, tidToWorker, sampleRateBytes });
    const t7 = r.perTask.get(7);
    expect(t7, "estimated: missing task 7").toBeTruthy();
    // weight(1000) = 1000 / (1 - exp(-1)) ≈ 1581.98
    const expectedPerSample = 1000 / (1 - Math.exp(-1));
    expect(
      Math.abs(t7.estimatedBytes - 3 * expectedPerSample),
      `estimated: expected ~${3 * expectedPerSample}, got ${t7.estimatedBytes}`,
    ).toBeLessThanOrEqual(1);
    expect(r.sampleRateBytes, "estimated: sampleRateBytes not returned").toBe(1000);
    // For s >> R, weight ≈ s (large allocs represent themselves)
    const bigAllocs = [
      { timestamp: 100, tid: 10, size: 100000, addr: "0x1", callchain: [] },
    ];
    const r2 = analyzeAllocations(bigAllocs, [], { events, tidToWorker, sampleRateBytes });
    const t7b = r2.perTask.get(7);
    expect(
      Math.abs(t7b.estimatedBytes - 100000),
      `large alloc: weight should ≈ size, got ${t7b.estimatedBytes}`,
    ).toBeLessThanOrEqual(10);
  });

  it("address reuse: only the matching alloc is considered freed", () => {
    // Two allocs at the same address (reuse after free). Only the first is freed.
    const allocs = [
      { timestamp: 100, tid: 10, size: 1024, addr: "0x1", callchain: ["0xa"] },
      { timestamp: 300, tid: 10, size: 2048, addr: "0x1", callchain: ["0xa"] }, // reused addr
    ];
    const frees = [
      { timestamp: 200, tid: 10, addr: "0x1", size: 1024, allocTimestampNs: 100 }, // frees first alloc only
    ];
    const r = analyzeAllocations(allocs, frees);
    expect(
      r.summary.leakedCount,
      `addressReuse: expected 1 leak, got ${r.summary.leakedCount}`,
    ).toBe(1);
    expect(r.leaks.length, `addressReuse: expected 1 leak entry, got ${r.leaks.length}`).toBe(1);
    expect(
      r.leaks[0].timestamp,
      `addressReuse: leaked alloc should be the second one (t=300)`,
    ).toBe(300);
  });
});

// ── heap flamegraph from alloc events ──

describe("heap flamegraph from alloc events", () => {
  it("alloc events produce valid flamegraph tree", () => {
    // Simulate the viewer's heap flamegraph: convert alloc events to samples
    // and build a flamegraph tree from them.
    const allocEvents = [
      { timestamp: 100, tid: 10, size: 1024, addr: "0x1", callchain: ["0xaaa", "0xbbb", "0xccc"] },
      { timestamp: 200, tid: 10, size: 2048, addr: "0x2", callchain: ["0xaaa", "0xbbb", "0xddd"] },
      { timestamp: 300, tid: 10, size: 512, addr: "0x3", callchain: ["0xaaa", "0xeee"] },
    ];
    const samples = allocEvents
      .filter((a) => a.callchain.length > 0)
      .map((a) => ({ callchain: a.callchain, workerId: 0 }));
    const symbols = new Map(); // no symbols — raw addresses used as names
    const tree = buildFlamegraphTree(samples, symbols);
    expect(tree.count, `heapFg: root count should be 3, got ${tree.count}`).toBe(3);
    // All samples share "0xaaa" as the bottom frame (reversed: it becomes the first child)
    // buildFlamegraphTree reverses callchains, so 0xccc/0xddd/0xeee are at the bottom
    // and 0xaaa is at the top of the tree
    expect(
      tree.children.has("0xccc") || tree.children.has("0xaaa"),
      // The tree reverses callchains, so the deepest frame becomes the root child
      "heapFg: expected reversed callchain structure in tree",
    ).toBe(true);
    const flat = flattenFlamegraph(tree, tree.count);
    expect(flat.nodes.length, "heapFg: flattenFlamegraph produced no nodes").toBeGreaterThan(0);
    expect(flat.maxDepth, "heapFg: expected depth > 0").toBeGreaterThanOrEqual(1);
  });

  it("empty callchains filtered out correctly", () => {
    // Alloc events with empty callchains should be filtered out
    const allocEvents = [
      { timestamp: 100, tid: 10, size: 1024, addr: "0x1", callchain: [] as string[] },
      { timestamp: 200, tid: 10, size: 2048, addr: "0x2", callchain: ["0xaaa"] },
    ];
    const samples = allocEvents
      .filter((a) => a.callchain.length > 0)
      .map((a) => ({ callchain: a.callchain, workerId: 0 }));
    expect(
      samples.length,
      `heapFgEmpty: expected 1 sample after filter, got ${samples.length}`,
    ).toBe(1);
    const tree = buildFlamegraphTree(samples, new Map());
    expect(tree.count, `heapFgEmpty: root count should be 1, got ${tree.count}`).toBe(1);
  });
});
