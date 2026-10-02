// warmDerived writes the same caches the lazy path fills on first render.
// They are separate code, so the two have to be held to the same output field
// for field.

import { readFileSync } from "node:fs";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTrace } from "../../../trace_parser.js";
import type { ParsedTrace } from "../../../trace_parser.js";
import { ColumnarEvents } from "./columnar-events.js";
import { ColumnarSpanEvents } from "./columnar-span-events.js";
import {
  TRACE_DENSITY_RESOLUTION,
  lifecycleWorkerIds,
  traceDensityBins,
  sharedDetectorInputs,
  sharedSpanData,
  sharedWorkerSpans,
  warmDerived,
} from "./derived.js";

const tracePath = new URL("../../../public/demo-trace.bin", import.meta.url);

async function load(): Promise<ParsedTrace> {
  // Both sinks, the way load.ts wires them: the columnar lanes and the columnar
  // span store go together.
  const spanEventSink = new ColumnarSpanEvents();
  const trace = await parseTrace(readFileSync(tracePath), {
    eventSink: new ColumnarEvents(),
    spanEventSink,
  } as never);
  (trace as { spanEvents?: unknown }).spanEvents = spanEventSink;
  return trace;
}

describe("warmDerived", () => {
  let warmed: ParsedTrace;
  let lazy: ParsedTrace;
  let fractions: number[];

  beforeAll(async () => {
    warmed = await load();
    lazy = await load();
    fractions = [];
    await warmDerived(
      warmed,
      (f) => fractions.push(f),
      () => Promise.resolve(),
    );
  });

  it("reports a non-decreasing 0..1 fraction", () => {
    expect(fractions.at(-1)).toBe(1);
    for (const f of fractions) {
      expect(f).toBeGreaterThanOrEqual(0);
      expect(f).toBeLessThanOrEqual(1);
    }
    for (let i = 1; i < fractions.length; i++) {
      expect(fractions[i]!).toBeGreaterThanOrEqual(fractions[i - 1]!);
    }
  });

  it("produces the worker spans the lazy path would have built", () => {
    const a = sharedWorkerSpans(warmed);
    const b = sharedWorkerSpans(lazy);
    expect(a.maxLocalQueue).toBe(b.maxLocalQueue);
    expect(a.queueSamples).toEqual(b.queueSamples);
    expect(Object.keys(a.workerSpans).sort()).toEqual(
      Object.keys(b.workerSpans).sort(),
    );
    for (const w of lifecycleWorkerIds(lazy)) {
      const la = a.workerSpans[w];
      const lb = b.workerSpans[w];
      expect(la?.polls.length).toBe(lb?.polls.length);
      expect(la?.parks.length).toBe(lb?.parks.length);
      expect(la?.actives.length).toBe(lb?.actives.length);
      expect([...(la?.polls ?? [])]).toEqual([...(lb?.polls ?? [])]);
      expect([...(la?.parks ?? [])]).toEqual([...(lb?.parks ?? [])]);
      expect([...(la?.actives ?? [])]).toEqual([...(lb?.actives ?? [])]);
      const wa = a.queueSampleIndex.forWorker(w);
      const wb = b.queueSampleIndex.forWorker(w);
      expect(wa.length).toBe(wb.length);
      expect(wa.toRecords()).toEqual(wb.toRecords());
    }
  });

  it("produces the detector inputs the lazy path would have built", () => {
    const a = sharedDetectorInputs(warmed);
    const b = sharedDetectorInputs(lazy);
    expect(a.workerIds).toEqual(b.workerIds);
    expect(a.hasWorkerCpuTime).toBe(b.hasWorkerCpuTime);
    expect(a.schedDelays.length).toBe(b.schedDelays.length);
    const keys = (list: typeof a.schedDelays): string[] =>
      [...list].map(
        (d) => `${d.wakeTime}:${d.pollTime}:${d.delay}:${d.taskId}:${d.worker}`,
      );
    expect(keys(a.schedDelays)).toEqual(keys(b.schedDelays));
  });

  it("is a no-op once both caches are populated", async () => {
    const calls: number[] = [];
    await warmDerived(warmed, (f) => calls.push(f), () => Promise.resolve());
    expect(calls).toEqual([]);
  });
});

describe("span-event release", () => {
  it("drops the columns once the span store is built, and stays usable", async () => {
    const trace = await load();
    const store = trace.spanEvents as unknown as {
      length: number;
      released: boolean;
      spanIdAt(i: number): string;
    };
    expect(store.length).toBeGreaterThan(0);
    expect(store.released).toBe(false);

    const data = sharedSpanData(trace);
    expect(data.columnarSpans?.length).toBeGreaterThan(0);
    expect(store.released).toBe(true);
    expect(() => store.spanIdAt(0)).toThrow(/released/);

    // The cached span data is what consumers read, and it still answers.
    expect(sharedSpanData(trace)).toBe(data);
    expect(sharedSpanData(trace).columnarSpans?.length).toBe(
      data.columnarSpans?.length,
    );
  });
});

// The event columns go once the reconstruction has read them, but the minimap's
// density strip and the lifecycle worker-id scan walk them too. Both have to be
// memoized first, on either path that builds the worker spans, or they answer
// from empty columns without saying so.
describe("event-column release", () => {
  const eventsOf = (t: ParsedTrace): ColumnarEvents =>
    t.events as unknown as ColumnarEvents;

  it("releases via the lazy path with every dependent read already memoized", async () => {
    const trace = await load();
    const ev = eventsOf(trace);
    const len = ev.length;
    expect(ev.released).toBe(false);

    sharedWorkerSpans(trace);
    expect(ev.released).toBe(true);

    // Survives: the toolbar's count and the axis bounds read these, not columns.
    expect(ev.length).toBe(len);
    expect(trace.minTs).not.toBeNull();

    // Memoized before the release, so they still answer.
    expect(lifecycleWorkerIds(trace).length).toBeGreaterThan(0);
    expect(traceDensityBins(trace)).not.toBeNull();
    expect(traceDensityBins(trace)!.length).toBe(TRACE_DENSITY_RESOLUTION);

    // A reader that outlives the release fails loudly.
    expect(() => [...ev]).toThrow(/released/);
    expect(() => ev.at(0)).toThrow(/released/);
  });

  it("releases via warmDerived with the same reads memoized", async () => {
    const trace = await load();
    const ev = eventsOf(trace);
    await warmDerived(trace, () => {}, () => Promise.resolve());
    expect(ev.released).toBe(true);
    expect(lifecycleWorkerIds(trace).length).toBeGreaterThan(0);
    expect(traceDensityBins(trace)).not.toBeNull();
  });

  it("agrees with the pre-release values", async () => {
    const before = await load();
    const ids = lifecycleWorkerIds(before);
    const bins = eventsOf(before).densityBins(
      before.minTs!, before.maxTs!, TRACE_DENSITY_RESOLUTION,
    );
    const after = await load();
    sharedWorkerSpans(after);
    expect(lifecycleWorkerIds(after)).toEqual(ids);
    expect(traceDensityBins(after)).toEqual(bins);
  });
});
