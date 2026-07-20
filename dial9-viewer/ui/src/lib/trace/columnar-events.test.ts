// Parity + unit tests for the columnar event store. The store must produce, for
// every event, EXACTLY the fields the fat-array parser produces - it is fed the
// same fat event objects via the parser's eventSink option, and every consumer
// reads through it.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "./load.js";
import { buildWorkerSpans } from "./analysis.js";
import { deriveWorkerIds } from "./derived.js";
import { ColumnarEvents, EVT, capacityForBytes } from "./columnar-events.js";
import type { EventLike } from "./columnar-events.js";

let raw: Uint8Array;

beforeAll(() => {
  const bytes = readFileSync(
    fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url))
  );
  raw =
    bytes[0] === 0x1f && bytes[1] === 0x8b
      ? new Uint8Array(gunzipSync(bytes))
      : new Uint8Array(bytes);
});

const FIELDS: (keyof EventLike)[] = [
  "eventType", "timestamp", "workerId", "localQueue", "globalQueue",
  "cpuTime", "schedWait", "taskId", "spawnLocId", "spawnLoc", "tid",
];

describe("ColumnarEvents parser sink parity", () => {
  it("produces byte-for-byte identical events to the fat-array parser", async () => {
    const fat = await parseTraceBuffer(raw);
    const col = await parseTraceBuffer(raw, {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      eventSink: new ColumnarEvents(),
    } as never);

    const store = col.events as unknown as ColumnarEvents;
    expect(store).toBeInstanceOf(ColumnarEvents);
    expect(store.length).toBe(fat.events.length);
    expect(fat.events.length).toBeGreaterThan(0);
    expect(col.minTs).toBe(fat.minTs);
    expect(col.maxTs).toBe(fat.maxTs);

    // Field-by-field over every event (materialized, safe to compare).
    for (let i = 0; i < fat.events.length; i++) {
      const a = fat.events[i] as unknown as EventLike;
      const b = store.at(i)!;
      for (const f of FIELDS) {
        // Normalize undefined/null tid the same way (fat leaves tid undefined
        // on non-park events; store decodes NaN -> undefined).
        expect(b[f], `event ${i} field ${String(f)}`).toStrictEqual(
          normalize(f, a[f])
        );
      }
    }
  });
});

// The fat parser omits `tid` (undefined) on events that never set it, and sets
// schedWait null on unsampled unparks; the store round-trips both identically.
function normalize(f: keyof EventLike, v: unknown): unknown {
  if (f === "tid" && v === undefined) return undefined;
  return v;
}

describe("buildWorkerSpans columnar parity", () => {
  it("columnar store yields the same worker spans as the frozen fat path", async () => {
    const fat = await parseTraceBuffer(raw);
    const col = await parseTraceBuffer(raw, { eventSink: new ColumnarEvents() } as never);

    // Same workerIds + gaps + maxTs for both, isolating buildWorkerSpans parity.
    const workerIds = deriveWorkerIds(fat);
    const maxTs = fat.maxTs ?? 0;

    const a = buildWorkerSpans(fat.events, workerIds, maxTs, fat.blockInPlaceGaps);
    const b = buildWorkerSpans(
      col.events as never,
      workerIds,
      maxTs,
      col.blockInPlaceGaps
    );

    // perWorker is intentionally divergent (indices vs objects) and unread.
    expect(b.workerSpans).toStrictEqual(a.workerSpans);
    expect(b.queueSamples).toStrictEqual(a.queueSamples);
    expect(b.workerQueueSamples).toStrictEqual(a.workerQueueSamples);
    expect(b.maxLocalQueue).toBe(a.maxLocalQueue);
    expect(b.wakesByTask).toStrictEqual(a.wakesByTask);
    expect(b.wakesByWorker).toStrictEqual(a.wakesByWorker);
  });
});

describe("ColumnarEvents store mechanics", () => {
  function build(): ColumnarEvents {
    const c = new ColumnarEvents(2); // tiny cap to exercise grow()
    c.push(mk({ eventType: EVT.PollStart, timestamp: 10, workerId: 1, taskId: 7, spawnLoc: "a::b", localQueue: 3 }));
    c.push(mk({ eventType: EVT.WorkerUnpark, timestamp: 20, workerId: 2, cpuTime: 99, schedWait: null, tid: 55 }));
    c.push(mk({ eventType: EVT.WorkerPark, timestamp: 30, workerId: 2, tid: 55, cpuTime: 5, schedWait: 0 }));
    c.push(mk({ eventType: EVT.WakeEvent, timestamp: 40, workerId: 3, wakerTaskId: 1, wokenTaskId: 2 }));
    return c;
  }

  it("grows past initial capacity and preserves order", () => {
    const c = build();
    expect(c.length).toBe(4);
    // NB: use .map (materializing) not [...c] spread - the flyweight iterator
    // yields one reused object, so spreading would collapse to 4 identical refs.
    expect(c.map((e) => e.timestamp)).toEqual([10, 20, 30, 40]);
  });

  it("decodes sentinels: schedWait null vs 0, tid undefined, spawnLoc intern", () => {
    const c = build();
    const [poll, unpark, park] = [c.at(0)!, c.at(1)!, c.at(2)!];
    expect(poll.spawnLoc).toBe("a::b");
    expect(poll.spawnLocId).toBe("a::b");
    expect(poll.tid).toBeUndefined(); // poll never set tid
    expect(unpark.schedWait).toBeNull(); // unsampled
    expect(park.schedWait).toBe(0); // sampled zero, distinct from null
    expect(unpark.tid).toBe(55);
    expect(c.spawnList).toEqual(["a::b"]);
  });

  it("flyweight iteration reuses ONE object (fields still correct per step)", () => {
    const c = build();
    const seen = new Set<object>();
    const times: number[] = [];
    for (const e of c) {
      seen.add(e as object);
      times.push(e.timestamp);
    }
    expect(seen.size).toBe(1); // same flyweight every step
    expect(times).toEqual([10, 20, 30, 40]); // yet fields read live per step
  });

  it(".at(i) materializes independent objects (safe to retain)", () => {
    const c = build();
    const a = c.at(0)!;
    const b = c.at(1)!;
    expect(a).not.toBe(b);
    expect(a.timestamp).toBe(10);
    expect(b.timestamp).toBe(20);
    expect(c.at(-1)!.timestamp).toBe(40);
    expect(c.at(99)).toBeUndefined();
  });

  it(".map / .filter / .some read fields correctly", () => {
    const c = build();
    expect(c.map((e) => e.timestamp)).toEqual([10, 20, 30, 40]);
    expect(c.filter((e) => e.eventType === EVT.WorkerPark).map((e) => e.timestamp)).toEqual([30]);
    expect(c.some((e) => e.eventType === EVT.WakeEvent)).toBe(true);
    expect(c.some((e) => e.eventType === 123)).toBe(false);
  });
});

describe("ColumnarEvents windowing substrate (tsIndex + windowRange)", () => {
  function build(): ColumnarEvents {
    // Deliberately out-of-order ts (wire order), incl. a full-span outlier.
    const c = new ColumnarEvents(2);
    const tss = [10, 20, 15, 40, 5, 30, 25, 35];
    for (const t of tss) c.push(mk({ eventType: EVT.PollStart, timestamp: t, workerId: 1 }));
    return c;
  }

  it("tsIndex sorts indices ascending by ts", () => {
    const c = build();
    const perm = c.tsIndex();
    const sortedTs = [...perm].map((i) => c.ts[i]);
    expect(sortedTs).toEqual([5, 10, 15, 20, 25, 30, 35, 40]);
  });

  it("windowRange covers EXACTLY the in-window events, regardless of wire order", () => {
    const c = build();
    for (const [t0, t1] of [[12, 28], [5, 40], [0, 100], [21, 21], [100, 200], [-5, 4]] as [number, number][]) {
      const { lo, hi } = c.windowRange(t0, t1);
      const perm = c.tsIndex();
      const windowed = [];
      for (let k = lo; k < hi; k++) windowed.push(c.ts[perm[k]!]);
      // Brute-force truth: every event's ts in [t0,t1], sorted.
      const truth = [];
      for (let i = 0; i < c.length; i++) if (c.ts[i]! >= t0 && c.ts[i]! <= t1) truth.push(c.ts[i]!);
      truth.sort((a, b) => a - b);
      expect(windowed, `window [${t0},${t1}]`).toEqual(truth);
    }
  });

  it("windowRange matches a brute-force filter on the real demo trace", async () => {
    const store = new ColumnarEvents();
    const trace = await parseTraceBuffer(raw, { eventSink: store } as never);
    const s = trace.events as unknown as ColumnarEvents;
    const min = s.minTs, max = s.maxTs, span = max - min;
    // A few interior windows.
    for (const [f0, f1] of [[0.2, 0.4], [0.45, 0.55], [0, 1], [0.9, 1.1]] as [number, number][]) {
      const t0 = min + span * f0, t1 = min + span * f1;
      const { lo, hi } = s.windowRange(t0, t1);
      let truth = 0;
      for (let i = 0; i < s.length; i++) if (s.ts[i]! >= t0 && s.ts[i]! <= t1) truth++;
      expect(hi - lo, `window frac [${f0},${f1}]`).toBe(truth);
    }
  });
});

describe("ColumnarEvents.densityBins (overview from the ts column)", () => {
  it("is bin-for-bin identical to binTimestamps over the mapped timestamps", async () => {
    const { binTimestamps } = await import("../../pages/viewer/minimap-model.js");
    const store = new ColumnarEvents();
    const trace = await parseTraceBuffer(raw, { eventSink: store } as never);
    const s = trace.events as unknown as ColumnarEvents;
    const startNs = s.minTs, endNs = s.maxTs;
    for (const res of [256, 64, 1000]) {
      const direct = s.densityBins(startNs, endNs, res);
      const viaMap = binTimestamps(s.map((e) => e.timestamp), { startNs, endNs }, res);
      expect(direct, `res ${res}`).toEqual(viaMap);
    }
  });
});

function mk(o: Partial<EventLike>): EventLike {
  return {
    eventType: 0, timestamp: 0, workerId: 0, localQueue: 0, globalQueue: 0,
    cpuTime: 0, schedWait: 0, taskId: 0, spawnLocId: null, spawnLoc: null,
    tid: undefined, wakerTaskId: 0, wokenTaskId: 0, targetWorker: 0, ...o,
  };
}

describe("capacityForBytes", () => {
  it("scales with the byte count", () => {
    // ~32 bytes/event, so a 32MB trace should pre-size near 1M events.
    expect(capacityForBytes(32 * 1024 * 1024)).toBe(1024 * 1024);
  });

  it("never starts below the default capacity", () => {
    expect(capacityForBytes(1)).toBe(1 << 16);
    expect(capacityForBytes(0)).toBe(1 << 16);
  });

  it("is defensive about a nonsense byte count", () => {
    expect(capacityForBytes(Number.NaN)).toBe(1 << 16);
    expect(capacityForBytes(-5)).toBe(1 << 16);
  });

  it("clamps so a huge trace cannot commit unbounded columns up front", () => {
    expect(capacityForBytes(Number.MAX_SAFE_INTEGER)).toBe(1 << 25);
  });

  it("pre-sized columns hold the same data as default-sized ones", () => {
    const a = new ColumnarEvents();
    const b = new ColumnarEvents(capacityForBytes(4 * 1024 * 1024));
    for (const sink of [a, b]) {
      for (let i = 0; i < 200; i++) {
        sink.pushEvent(
          EVT.PollStart, i * 10, i % 4, 0, 0, 0, null, i, null,
          undefined, undefined, undefined,
        );
      }
    }
    expect(b.length).toBe(a.length);
    expect(b.at(199)!.timestamp).toBe(a.at(199)!.timestamp);
    expect(b.at(199)!.timestamp).toBe(1990);
    expect(b.at(199)!.taskId).toBe(199);
  });
});
