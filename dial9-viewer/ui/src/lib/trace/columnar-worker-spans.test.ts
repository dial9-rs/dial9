import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "./load.js";
import { buildWorkerSpans } from "./analysis.js";
import { deriveWorkerIds } from "./derived.js";
import { ColumnarWorkerSpans } from "./columnar-worker-spans.js";

let ws: Record<number, unknown>;
let trace: Awaited<ReturnType<typeof parseTraceBuffer>>;

beforeAll(async () => {
  const b = readFileSync(fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)));
  const raw = b[0] === 0x1f && b[1] === 0x8b ? new Uint8Array(gunzipSync(b)) : new Uint8Array(b);
  trace = await parseTraceBuffer(raw);
  ws = buildWorkerSpans(trace.events, deriveWorkerIds(trace), trace.maxTs ?? 0, trace.blockInPlaceGaps)
    .workerSpans as never;
});

describe("ColumnarWorkerSpans", () => {
  it("columnar polls are field-for-field identical to the object polls", () => {
    const store = ColumnarWorkerSpans.fromWorkerSpans(ws as never);
    let total = 0;
    for (const w of store.workers()) {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const polls = (ws as any)[w].polls as any[];
      expect(store.pollCount(w)).toBe(polls.length);
      for (let i = 0; i < polls.length; i++) {
        const p = polls[i];
        const v = store.pollAt(w, i)!;
        expect(v.start).toBe(p.start);
        expect(v.end).toBe(p.end);
        expect(v.taskId).toBe(p.taskId);
        expect(v.spawnLoc).toBe(p.spawnLoc ?? null);
        expect(v.openEnded).toBe(!!p.openEnded);
        total++;
      }
    }
    expect(total).toBeGreaterThan(0);
  });

  it("columnar parks + actives are field-for-field identical to the objects", () => {
    const store = ColumnarWorkerSpans.fromWorkerSpans(ws as never);
    let parks = 0, actives = 0;
    for (const w of store.workers()) {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const lane = (ws as any)[w];
      expect(store.parkCount(w)).toBe(lane.parks.length);
      expect(store.activeCount(w)).toBe(lane.actives.length);
      for (let i = 0; i < lane.parks.length; i++) {
        const v = store.parkAt(w, i)!;
        expect(v.start).toBe(lane.parks[i].start);
        expect(v.end).toBe(lane.parks[i].end);
        expect(v.schedWait).toBe(lane.parks[i].schedWait ?? null);
        parks++;
      }
      for (let i = 0; i < lane.actives.length; i++) {
        const v = store.activeAt(w, i)!;
        expect(v.start).toBe(lane.actives[i].start);
        expect(v.end).toBe(lane.actives[i].end);
        expect(v.ratio).toBe(lane.actives[i].ratio);
        actives++;
      }
    }
    expect(parks + actives).toBeGreaterThan(0);
  });

  it("windowRange returns exactly the polls intersecting [t0,t1]", () => {
    const store = ColumnarWorkerSpans.fromWorkerSpans(ws as never);
    const w = store.workers().find((x) => store.pollCount(x) > 4)!;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const polls = (ws as any)[w].polls as any[];
    const min = trace.minTs!, max = trace.maxTs!, span = max - min;
    for (const [f0, f1] of [[0.2, 0.4], [0.45, 0.55], [0, 1], [-0.1, 1.1]] as [number, number][]) {
      const t0 = min + span * f0, t1 = min + span * f1;
      const { lo, hi } = store.windowRange(w, t0, t1);
      const got = new Set<number>();
      for (let i = lo; i < hi; i++) got.add(i);
      const truth = new Set<number>();
      for (let i = 0; i < polls.length; i++) if (polls[i].end >= t0 && polls[i].start <= t1) truth.add(i);
      expect([...got].sort((a, b) => a - b), `window [${f0},${f1}]`).toEqual([...truth].sort((a, b) => a - b));
    }
  });

  it("mipmap emits one representative (longest) per bucket, counts summing to the window total", () => {
    const store = ColumnarWorkerSpans.fromWorkerSpans(ws as never);
    const w = store.workers().find((x) => store.pollCount(x) > 4)!;
    const t0 = trace.minTs!, t1 = trace.maxTs!;
    const res = Math.max(1, Math.floor((t1 - t0) / 8)); // ~8 buckets
    const buckets = store.mipmap(w, t0, t1, res);
    const { lo, hi } = store.windowRange(w, t0, t1);
    const summed = buckets.reduce((s, b) => s + b.count, 0);
    expect(summed).toBe(hi - lo); // every in-window poll counted once
    // Each representative is the longest in its bucket.
    for (const b of buckets) {
      const v = store.pollAt(w, b.idx)!;
      expect(v.end - v.start).toBe(b.dur);
    }
  });
});

describe("ColumnarWorkerSpans.attachCpuSamples matches the frozen attachCpuSamples", () => {
  it("same per-poll cpu/sched samples, cpuSampleTimes, spawnLoc/inPoll, and counts", async () => {
    const b = readFileSync(fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)));
    const raw = b[0] === 0x1f && b[1] === 0x8b ? new Uint8Array(gunzipSync(b)) : new Uint8Array(b);

    // Independent parses: A = fat cpuSamples, B = columnar CpuSample store.
    const { attachCpuSamples } = await import("./index.js");
    const { ColumnarCpuSamples } = await import("./columnar-cpu-samples.js");

    const fatTrace = await parseTraceBuffer(raw);
    const wids = deriveWorkerIds(fatTrace);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const wsA: any = buildWorkerSpans(fatTrace.events, wids, fatTrace.maxTs ?? 0, fatTrace.blockInPlaceGaps).workerSpans;
    const countsA = attachCpuSamples(fatTrace.cpuSamples, wsA);

    const cpuStore = new ColumnarCpuSamples();
    const colTrace = await parseTraceBuffer(raw, { cpuSampleSink: cpuStore } as never);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const wsB: any = buildWorkerSpans(colTrace.events, wids, colTrace.maxTs ?? 0, colTrace.blockInPlaceGaps).workerSpans;
    const store = ColumnarWorkerSpans.fromWorkerSpans(wsB);
    const countsB = store.attachCpuSamples(cpuStore.samples);

    expect(countsB).toEqual(countsA);
    expect(countsA.pollsWithCpuSamples + countsA.pollsWithSchedSamples).toBeGreaterThan(0);

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const tsOf = (arr: any[] | undefined) => (arr ?? []).map((s) => s.timestamp);
    let checkedCpu = 0, checkedSched = 0;
    for (const w of wids) {
      const polls = wsA[w].polls;
      expect(store.pollCount(w)).toBe(polls.length);
      // cpuSampleTimes parity (same order the frozen pass pushed them).
      expect(store.cpuSampleTimes(w)).toEqual(wsA[w].cpuSampleTimes);
      for (let i = 0; i < polls.length; i++) {
        const v = store.pollAt(w, i)!;
        // Same sample sequence (by timestamp, same within-poll order).
        expect(tsOf(v.cpuSamples), `w${w} poll${i} cpu`).toEqual(tsOf(polls[i].cpuSamples));
        expect(tsOf(v.schedSamples), `w${w} poll${i} sched`).toEqual(tsOf(polls[i].schedSamples));
        if (v.cpuSamples) checkedCpu++;
        if (v.schedSamples) checkedSched++;
      }
    }
    expect(checkedCpu).toBe(countsA.pollsWithCpuSamples);
    expect(checkedSched).toBe(countsA.pollsWithSchedSamples);

    // spawnLoc/inPoll written onto the samples themselves, index-for-index
    // (both parses see the same wire order).
    expect(cpuStore.samples.length).toBe(fatTrace.cpuSamples.length);
    for (let i = 0; i < cpuStore.samples.length; i++) {
      expect(cpuStore.samples[i]!.spawnLoc, `sample ${i} spawnLoc`).toBe(fatTrace.cpuSamples[i]!.spawnLoc);
      expect(cpuStore.samples[i]!.inPoll, `sample ${i} inPoll`).toBe(fatTrace.cpuSamples[i]!.inPoll);
    }
  });
});

describe("buildWorkerSpansColumnarStore builds the store DIRECTLY, identical to the fat->bridge store", () => {
  it("every poll/park/active field matches, and the non-span aggregates match the fat build", async () => {
    const b = readFileSync(fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)));
    const raw = b[0] === 0x1f && b[1] === 0x8b ? new Uint8Array(gunzipSync(b)) : new Uint8Array(b);
    const { buildWorkerSpansColumnarStore } = await import("./worker-spans-columnar.js");
    const { ColumnarEvents } = await import("./columnar-events.js");

    const colTrace = await parseTraceBuffer(raw, { eventSink: new ColumnarEvents() } as never);
    const wids = deriveWorkerIds(colTrace);
    const direct = buildWorkerSpansColumnarStore(
      colTrace.events as never, wids, colTrace.maxTs ?? 0, colTrace.blockInPlaceGaps
    );

    // Reference: fat build (frozen) -> bridge store.
    const fatTrace = await parseTraceBuffer(raw);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const fatWs: any = buildWorkerSpans(fatTrace.events, wids, fatTrace.maxTs ?? 0, fatTrace.blockInPlaceGaps);
    const bridge = ColumnarWorkerSpans.fromWorkerSpans(fatWs.workerSpans);

    let total = 0;
    for (const w of wids) {
      expect(direct.store.pollCount(w), `w${w} pollCount`).toBe(bridge.pollCount(w));
      expect(direct.store.parkCount(w), `w${w} parkCount`).toBe(bridge.parkCount(w));
      expect(direct.store.activeCount(w), `w${w} activeCount`).toBe(bridge.activeCount(w));
      const np = bridge.pollCount(w);
      for (const i of [0, np >> 1, np - 1].filter((x) => x >= 0 && x < np)) {
        const d = direct.store.pollAt(w, i)!, r = bridge.pollAt(w, i)!;
        expect([d.start, d.end, d.taskId, d.spawnLoc, d.openEnded])
          .toEqual([r.start, r.end, r.taskId, r.spawnLoc, r.openEnded]);
        total++;
      }
      const nk = bridge.parkCount(w);
      for (const i of [0, nk >> 1, nk - 1].filter((x) => x >= 0 && x < nk)) {
        const d = direct.store.parkAt(w, i)!, r = bridge.parkAt(w, i)!;
        expect([d.start, d.end, d.schedWait]).toEqual([r.start, r.end, r.schedWait]);
      }
      const na = bridge.activeCount(w);
      for (const i of [0, na >> 1, na - 1].filter((x) => x >= 0 && x < na)) {
        const d = direct.store.activeAt(w, i)!, r = bridge.activeAt(w, i)!;
        expect([d.start, d.end, d.ratio]).toEqual([r.start, r.end, r.ratio]);
      }
    }
    expect(total).toBeGreaterThan(0);
    // Non-span aggregates match the fat build.
    expect(direct.maxLocalQueue).toBe(fatWs.maxLocalQueue);
    expect(direct.queueSamples).toEqual(fatWs.queueSamples);
    expect(direct.workerQueueSamples).toEqual(fatWs.workerQueueSamples);
    expect(direct.wakesByTask).toEqual(fatWs.wakesByTask);
    expect(direct.wakesByWorker).toEqual(fatWs.wakesByWorker);
  });
});

describe("ColumnarWorkerSpans downsample matches frozen pixelDownsampleSpans", () => {
  it("polls: identical representatives across zoom levels, with/without selectedTask", async () => {
    const { pixelDownsampleSpans } = await import("../canvas/index.js");
    const store = ColumnarWorkerSpans.fromWorkerSpans(ws as never);
    const w = store.workers().find((x) => store.pollCount(x) > 100)!;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const polls = (ws as any)[w].polls as any[];
    const min = trace.minTs!, max = trace.maxTs!, span = max - min;
    const someTask = polls.find((p) => p.taskId)?.taskId ?? null;

    for (const [f0, f1] of [[0, 1], [0.25, 0.75], [0.4, 0.45], [0.49, 0.51]] as [number, number][]) {
      const vs = min + span * f0, ve = min + span * f1;
      for (const pw of [1500, 300, 40]) {
        for (const sel of [null, someTask]) {
          const startIdx = store.firstVisiblePoll(w, vs);
          const weight = sel
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            ? (s: any) => (s.taskId === sel ? Infinity : s.start)
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            : (s: any) => s.start;
          const fat = pixelDownsampleSpans(polls, startIdx, vs, ve, pw, weight);
          const col = store.downsamplePolls(w, startIdx, vs, ve, pw, sel);
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          const key = (s: any) => `${s.start}:${s.end}:${s.taskId}`;
          expect(col.map(key), `view[${f0},${f1}] pw${pw} sel${sel}`).toEqual(fat.map(key));
        }
      }
    }
  });

  it("actives: identical longest-per-column representatives", async () => {
    const { pixelDownsampleSpans } = await import("../canvas/index.js");
    const store = ColumnarWorkerSpans.fromWorkerSpans(ws as never);
    const w = store.workers().find((x) => store.activeCount(x) > 50)!;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const actives = (ws as any)[w].actives as any[];
    const min = trace.minTs!, max = trace.maxTs!, span = max - min;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const dur = (s: any) => s.end - s.start;
    for (const [f0, f1] of [[0, 1], [0.3, 0.6]] as [number, number][]) {
      const vs = min + span * f0, ve = min + span * f1;
      for (const pw of [1500, 80]) {
        const startIdx = store.firstVisibleActive(w, vs);
        const fat = pixelDownsampleSpans(actives, startIdx, vs, ve, pw, dur);
        const col = store.downsampleActives(w, startIdx, vs, ve, pw);
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const key = (s: any) => `${s.start}:${s.end}:${s.ratio}`;
        expect(col.map(key), `view[${f0},${f1}] pw${pw}`).toEqual(fat.map(key));
      }
    }
  });
});

describe("ColumnarWorkerSpans.laneView flyweight matches the fat lane arrays", () => {
  it(".at / for..of / .find / findSpanAt agree with the fat polls,parks,actives", async () => {
    const { findSpanAt } = await import("./query.js");
    const store = ColumnarWorkerSpans.fromWorkerSpans(ws as never);
    const lanes = store.workerLanes();
    let checked = 0;
    for (const w of store.workers()) {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const fat = (ws as any)[w];
      const view = lanes[w]!;
      expect(view.polls.length).toBe(fat.polls.length);
      expect(view.parks.length).toBe(fat.parks.length);
      expect(view.actives.length).toBe(fat.actives.length);
      expect(view.cpuSampleTimes).toEqual(fat.cpuSampleTimes ?? []);

      // .at(i) field-parity on a sampling of indices (incl. first/last).
      const n = fat.polls.length;
      for (const i of [0, 1, (n >> 1), n - 2, n - 1].filter((x) => x >= 0 && x < n)) {
        const v = view.polls.at(i)!, p = fat.polls[i];
        expect([v.start, v.end, v.taskId, v.spawnLoc, v.openEnded])
          .toEqual([p.start, p.end, p.taskId, p.spawnLoc ?? null, !!p.openEnded]);
        checked++;
      }
      // for..of yields the same start sequence.
      const viaIter: number[] = [];
      for (const p of view.polls) viaIter.push(p.start);
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      expect(viaIter).toEqual(fat.polls.map((p: any) => p.start));

      // findSpanAt over the view == over the fat array, at a few timestamps.
      if (n > 0) {
        const min = trace.minTs!, max = trace.maxTs!, span = max - min;
        for (const f of [0.1, 0.33, 0.5, 0.77, 0.99]) {
          const ns = min + span * f;
          const a = findSpanAt(view.polls, ns);
          const b = findSpanAt(fat.polls, ns);
          expect(a?.start ?? null, `findSpanAt @${f}`).toBe(b?.start ?? null);
        }
      }
      // .find matches Array.prototype.find (first poll of a given task).
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const someTask = fat.polls.find((p: any) => p.taskId)?.taskId;
      if (someTask != null) {
        const vf = view.polls.find((p) => p.taskId === someTask);
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const ff = fat.polls.find((p: any) => p.taskId === someTask);
        expect(vf?.start).toBe(ff.start);
      }
    }
    expect(checked).toBeGreaterThan(0);
  });
});

describe("ColumnarWorkerSpans.schedulingDelays matches frozen computeSchedulingDelays", () => {
  it("same delays (wakeTime/pollTime/delay/taskId/wakerTaskId/worker/poll.end), same order", async () => {
    const { computeSchedulingDelays } = await import("./index.js");
    const wids = deriveWorkerIds(trace);
    const r = buildWorkerSpans(trace.events, wids, trace.maxTs ?? 0, trace.blockInPlaceGaps);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const wsLocal: any = r.workerSpans;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const fat = computeSchedulingDelays(wsLocal, wids, r.wakesByTask) as any[];

    const store = ColumnarWorkerSpans.fromWorkerSpans(wsLocal);
    const col = store.schedulingDelays(wids, r.wakesByTask as never);

    expect(col.length).toBe(fat.length);
    expect(fat.length).toBeGreaterThan(0);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const norm = (d: any) => ({
      wakeTime: d.wakeTime, pollTime: d.pollTime, delay: d.delay,
      taskId: d.taskId, wakerTaskId: d.wakerTaskId, worker: d.worker,
      pollStart: d.poll.start, pollEnd: d.poll.end,
    });
    expect(col.map(norm)).toEqual(fat.map(norm));
  });
});
