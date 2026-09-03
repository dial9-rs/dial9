// Golden test: store.pointsOfInterest (raw-column scan) must emit the same POIs
// as the frozen filterPointsOfInterest over the fat worker spans, for every
// detector type - so poi.ts / minimap-poi.ts can scan columns instead of
// materializing every poll from the flyweight lane views.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "./load.js";
import {
  CORE_SPAWN_DELAY_THRESHOLD_US,
  DEFAULT_SPAWN_DELAY_THRESHOLD_US,
  attachCpuSamples,
  buildWorkerSpans,
  computeSchedulingDelays,
  filterPointsOfInterest,
} from "./index.js";
import { deriveWorkerIds } from "./derived.js";
import { ColumnarWorkerSpans } from "./columnar-worker-spans.js";

// eslint-disable-next-line @typescript-eslint/no-explicit-any
let ws: any;
let store: ColumnarWorkerSpans;
let workerIds: number[];
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let fatSched: any[];
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let colSched: any[];
let taskInstrumented: Map<number, boolean>;
let taskSpawnTimes: Map<number, number>;

beforeAll(async () => {
  const b = readFileSync(fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)));
  const raw = b[0] === 0x1f && b[1] === 0x8b ? new Uint8Array(gunzipSync(b)) : new Uint8Array(b);
  const trace = await parseTraceBuffer(raw);
  taskSpawnTimes = trace.taskSpawnTimes;
  workerIds = deriveWorkerIds(trace);
  const r = buildWorkerSpans(trace.events, workerIds, trace.maxTs ?? 0, trace.blockInPlaceGaps);
  ws = r.workerSpans;
  attachCpuSamples(trace.cpuSamples, ws); // so the fat cpu-sampled detector sees samples
  store = ColumnarWorkerSpans.fromWorkerSpans(ws);
  store.attachCpuSamples(trace.cpuSamples as never);
  fatSched = computeSchedulingDelays(ws, workerIds, r.wakesByTask);
  colSched = store.schedulingDelays(workerIds, r.wakesByTask as never);
  // Synthesize an instrumentation map so "uninstrumented" has matches.
  taskInstrumented = new Map();
  let flip = false;
  for (const w of workerIds) for (const p of ws[w].polls) {
    if (!taskInstrumented.has(p.taskId)) taskInstrumented.set(p.taskId, (flip = !flip));
  }
});

// Multiset compare (sortByWorst can order equal-value ties differently between
// the column scan and the fat scan; the COUNT + set is what matters).
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const key = (p: any) => `${p.time}|${p.worker}|${p.type}|${p.value}|${p.span?.start}|${p.span?.end}`;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const bag = (arr: any[]) => arr.map(key).sort();

describe("store.pointsOfInterest matches frozen filterPointsOfInterest", () => {
  for (const type of [
    "long-poll", "sched", "cpu-sampled", "wake-delay", "uninstrumented",
    "spawn-delay", "off-cpu-poll", "off-cpu-active",
  ] as const) {
    it(`${type}: same POIs (time/worker/value/span)`, () => {
      const opts = {
        hasSchedWait: true, sortByWorst: true, taskInstrumented, taskSpawnTimes,
        // Forced on: both off-CPU detectors are gated on their input existing.
        hasOnCpuSamples: true, hasWorkerCpuTime: true,
      };
      const fat = filterPointsOfInterest(type, ws, workerIds, fatSched, opts);
      const col = store.pointsOfInterest(type, workerIds, colSched, opts);
      expect(col.length, `${type} count`).toBe(fat.length);
      expect(bag(col)).toEqual(bag(fat));
    });
  }

  // Separate implementations, so a configured threshold must move them
  // identically - not just the default one above.
  for (const thresholdUs of [0, 250, 5_000, 10_000_000]) {
    it(`spawn-delay: same POIs at threshold ${thresholdUs}us`, () => {
      const opts = {
        hasSchedWait: true,
        sortByWorst: true,
        taskInstrumented,
        taskSpawnTimes,
        spawnDelayThresholdUs: thresholdUs,
      };
      const fat = filterPointsOfInterest("spawn-delay", ws, workerIds, fatSched, opts);
      const col = store.pointsOfInterest("spawn-delay", workerIds, colSched, opts);
      expect(col.length, `count at ${thresholdUs}us`).toBe(fat.length);
      expect(bag(col)).toEqual(bag(fat));
    });
  }

  it("spawn-delay: a higher threshold never admits more points", () => {
    const at = (spawnDelayThresholdUs: number): number =>
      store.pointsOfInterest("spawn-delay", workerIds, colSched, {
        hasSchedWait: true, sortByWorst: true, taskInstrumented, taskSpawnTimes,
        spawnDelayThresholdUs,
      }).length;
    const counts = [0, 100, 1_000, 100_000].map(at);
    for (let i = 1; i < counts.length; i++) {
      expect(counts[i]!, `threshold step ${i}`).toBeLessThanOrEqual(counts[i - 1]!);
    }
    // Guards against a vacuous suite if the demo trace loses its spawn data.
    expect(counts[0]).toBeGreaterThan(0);
  });

  it("spawn-delay: emits nothing without the spawn map", () => {
    const opts = { hasSchedWait: true, sortByWorst: true, taskInstrumented };
    expect(store.pointsOfInterest("spawn-delay", workerIds, colSched, opts)).toEqual([]);
    expect(filterPointsOfInterest("spawn-delay", ws, workerIds, fatSched, opts)).toEqual([]);
  });

  it("spawn-delay: the frozen core's default threshold matches the TS one", () => {
    expect(CORE_SPAWN_DELAY_THRESHOLD_US).toBe(DEFAULT_SPAWN_DELAY_THRESHOLD_US);
  });
});

// The cap exists because "uninstrumented" matches essentially every poll on a
// lightly-instrumented trace - millions of points, enough to kill the tab
// before the rail renders. Truncating must not distort what the user is told.
describe("pointsOfInterest limit", () => {
  const opts = (extra: Record<string, unknown> = {}) => ({
    hasSchedWait: true,
    sortByWorst: true,
    taskInstrumented,
    ...extra,
  });

  it("returns the worst `limit` points, in the same order as uncapped", () => {
    let full = -1;
    const uncapped = store.pointsOfInterest(
      "uninstrumented", workerIds, colSched, opts({ onTotal: (n: number) => (full = n) }),
    );
    expect(uncapped.length).toBeGreaterThan(3);

    let matched = -1;
    const capped = store.pointsOfInterest(
      "uninstrumented", workerIds, colSched,
      opts({ limit: 3, onTotal: (n: number) => (matched = n) }),
    );

    expect(capped.length).toBe(3);
    expect(capped.map((p) => p.value)).toEqual(uncapped.slice(0, 3).map((p) => p.value));
    // The count reported is the TRUE match count, not the capped length.
    expect(matched).toBe(full);
    expect(matched).toBeGreaterThan(3);
  });

  it("holds across the compaction boundary (limit*2 triggers a compact)", () => {
    // Exercises the amortized path: the buffer fills to 2*limit, compacts, and
    // keeps going. A bug there shows up as the wrong survivors, not a crash.
    const uncapped = store.pointsOfInterest("uninstrumented", workerIds, colSched, opts());
    for (const limit of [1, 2, 5, 17]) {
      if (uncapped.length < limit) continue;
      const capped = store.pointsOfInterest(
        "uninstrumented", workerIds, colSched, opts({ limit }),
      );
      expect(capped.length, `limit=${limit}`).toBe(limit);
      expect(capped.map((p) => p.value), `limit=${limit}`).toEqual(
        uncapped.slice(0, limit).map((p) => p.value),
      );
    }
  });

  it("is a no-op when the match count is under the cap", () => {
    let matched = -1;
    const all = store.pointsOfInterest(
      "uninstrumented", workerIds, colSched,
      opts({ limit: 1_000_000, onTotal: (n: number) => (matched = n) }),
    );
    expect(all.length).toBe(matched);
  });

  it("caps by time order when not sorting by worst", () => {
    const uncapped = store.pointsOfInterest(
      "uninstrumented", workerIds, colSched, opts({ sortByWorst: false }),
    );
    if (uncapped.length < 4) return;
    const capped = store.pointsOfInterest(
      "uninstrumented", workerIds, colSched, opts({ sortByWorst: false, limit: 3 }),
    );
    expect(capped.map((p) => p.time)).toEqual(uncapped.slice(0, 3).map((p) => p.time));
  });
});

// taskAggregates reduces per-task poll stats straight off the columns; it must
// match a fat reduction over the same worker spans (the Tasks-tab index).
describe("taskAggregates matches a fat reduction", () => {
  it("agrees on count / total / longest / first-poll for every task", () => {
    const fat = new Map();
    for (const w of workerIds) {
      for (const p of ws[w].polls) {
        const dur = p.end - p.start;
        const a = fat.get(p.taskId) ?? {
          pollCount: 0, totalPollNs: 0, longestPollNs: 0,
          firstPollStart: Infinity, firstPollWorker: -1, workers: new Set(),
        };
        a.pollCount++;
        // An open-ended poll's duration is unknown, so it contributes to the
        // count and the worker set but not to total/longest.
        if (!p.openEnded) {
          a.totalPollNs += dur;
          if (dur > a.longestPollNs) a.longestPollNs = dur;
        }
        if (p.start < a.firstPollStart) { a.firstPollStart = p.start; a.firstPollWorker = w; }
        a.workers.add(w);
        fat.set(p.taskId, a);
      }
    }

    const cols = store.taskAggregates();
    expect(cols.length).toBe(fat.size);
    for (const agg of cols) {
      const f = fat.get(agg.taskId);
      expect(f, `task ${agg.taskId} present in fat`).toBeDefined();
      expect(agg.pollCount).toBe(f.pollCount);
      expect(agg.totalPollNs).toBe(f.totalPollNs);
      expect(agg.longestPollNs).toBe(f.longestPollNs);
      expect(agg.firstPollStart).toBe(f.firstPollStart);
      expect(agg.firstPollWorker).toBe(f.firstPollWorker);
      expect(agg.workerCount).toBe(f.workers.size);
    }
  });
});

// Over the demo trace the off-CPU detectors match nothing (workers ~97% on-CPU)
// or everything (every long poll is sample-free), so parity there is vacuous.
describe("off-CPU detectors: column scan matches the fat scan on crafted lanes", () => {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const synthetic: any = {
    0: {
      polls: [
        { start: 0, end: 5e6, taskId: 1, spawnLocId: null, spawnLoc: null },
        { start: 10e6, end: 10.5e6, taskId: 2, spawnLocId: null, spawnLoc: null },
        { start: 20e6, end: 30e6, taskId: 3, spawnLocId: null, spawnLoc: null, openEnded: true },
      ],
      parks: [],
      actives: [
        { start: 0, end: 10e6, ratio: 0.1 },
        { start: 20e6, end: 30e6, ratio: 0.9 },
        { start: 40e6, end: 40.5e6, ratio: 0 },
      ],
      cpuSampleTimes: [],
    },
  };
  const opts = { sortByWorst: true, hasOnCpuSamples: true, hasWorkerCpuTime: true };

  for (const type of ["off-cpu-active", "off-cpu-poll"] as const) {
    it(`${type}: same POIs, and not an empty result`, () => {
      const syntheticStore = ColumnarWorkerSpans.fromWorkerSpans(synthetic);
      const fat = filterPointsOfInterest(type, synthetic, [0], [], opts);
      const col = syntheticStore.pointsOfInterest(type, [0], [], opts);
      expect(fat.length, `${type} matched nothing, so parity would be vacuous`)
        .toBeGreaterThan(0);
      expect(col.length, `${type} count`).toBe(fat.length);
      expect(bag(col)).toEqual(bag(fat));
    });
  }

  // Fat twin covered by minimap-poi.test.ts.
  it("anyActiveCpuTime ignores the ratio a zero-length period fabricates", () => {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const lane = (actives: unknown[]): any => ({
      0: { polls: [], parks: [], actives, cpuSampleTimes: [] },
    });
    // wall 0 => the builders stamp ratio 1.0, which says nothing about CPU time.
    expect(
      ColumnarWorkerSpans.fromWorkerSpans(
        lane([{ start: 5e6, end: 5e6, ratio: 1 }, { start: 10e6, end: 20e6, ratio: 0 }]),
      ).anyActiveCpuTime(),
    ).toBe(false);
    expect(
      ColumnarWorkerSpans.fromWorkerSpans(
        lane([{ start: 10e6, end: 20e6, ratio: 0.4 }]),
      ).anyActiveCpuTime(),
    ).toBe(true);
  });

  // Sample exclusion is the one branch reading different structures per path
  // (`poll.cpuSamples` vs the `cpuOff` CSR), and no other case attaches a
  // sample - so both paths could agree while both were wrong.
  it("off-cpu-poll: both paths drop a long poll that caught an on-CPU sample", () => {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const lanes: any = {
      0: {
        polls: [
          { start: 0, end: 5e6, taskId: 1, spawnLocId: null, spawnLoc: null },
          { start: 10e6, end: 15e6, taskId: 2, spawnLocId: null, spawnLoc: null },
        ],
        parks: [], actives: [], cpuSampleTimes: [],
      },
    };
    // One on-CPU sample inside the FIRST poll only.
    const samples = [
      { timestamp: 2e6, workerId: 0, tid: 1, source: 0, callchain: ["0x1"], cpu: 0 },
    ];
    const store = ColumnarWorkerSpans.fromWorkerSpans(lanes);
    attachCpuSamples(samples as never, lanes);
    store.attachCpuSamples(samples as never);

    const opts2 = { sortByWorst: true, hasOnCpuSamples: true };
    const fat = filterPointsOfInterest("off-cpu-poll", lanes, [0], [], opts2);
    const col = store.pointsOfInterest("off-cpu-poll", [0], [], opts2);
    expect(fat.map((p) => p.span.start)).toEqual([10e6]);
    expect(bag(col)).toEqual(bag(fat));
  });
});
