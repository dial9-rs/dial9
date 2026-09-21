// warmDerived writes the same caches the lazy path fills on first render, and
// they are separate code rather than one calling the other, so the two have to
// be held to the same output field for field.

import { readFileSync } from "node:fs";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTrace } from "../../../trace_parser.js";
import type { ParsedTrace } from "../../../trace_parser.js";
import { ColumnarEvents } from "./columnar-events.js";
import {
  lifecycleWorkerIds,
  sharedDetectorInputs,
  sharedWorkerSpans,
  warmDerived,
} from "./derived.js";

const tracePath = new URL("../../../public/demo-trace.bin", import.meta.url);

async function load(): Promise<ParsedTrace> {
  return parseTrace(readFileSync(tracePath), {
    eventSink: new ColumnarEvents(),
  } as never);
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
    const key = (d: (typeof a.schedDelays)[number]): string =>
      `${d.wakeTime}:${d.pollTime}:${d.delay}:${d.taskId}:${d.worker}`;
    expect(a.schedDelays.map(key)).toEqual(b.schedDelays.map(key));
  });

  it("is a no-op once both caches are populated", async () => {
    const calls: number[] = [];
    await warmDerived(warmed, (f) => calls.push(f), () => Promise.resolve());
    expect(calls).toEqual([]);
  });
});
