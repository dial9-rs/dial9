// End-to-end check of the columnar big-trace path THROUGH the real entry points
// (sharedWorkerSpans / sharedSpanData in data.ts): a trace parsed with the
// columnar sinks must yield lane VIEWS + span data equivalent to the fat path.
// This is the flip the viewer takes for main-thread (big) traces; the unit
// suites otherwise only exercise the fat branch.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "../../../lib/trace/load.js";
import { ColumnarEvents } from "../../../lib/trace/columnar-events.js";
import { ColumnarCpuSamples } from "../../../lib/trace/columnar-cpu-samples.js";
import { ColumnarSpanEvents } from "../../../lib/trace/columnar-span-events.js";
import { sharedWorkerSpans, sharedSpanData, deriveWorkerIds } from "../../../lib/trace/derived.js";

let raw: Uint8Array;
beforeAll(() => {
  const b = readFileSync(fileURLToPath(new URL("../../../../public/demo-trace.bin", import.meta.url)));
  raw = b[0] === 0x1f && b[1] === 0x8b ? new Uint8Array(gunzipSync(b)) : new Uint8Array(b);
});

describe("columnar big-trace path via sharedWorkerSpans / sharedSpanData", () => {
  it("lane views + span data equal the fat path", async () => {
    const fat = await parseTraceBuffer(raw);
    const spanEventSink = new ColumnarSpanEvents();
    const col = await parseTraceBuffer(raw, {
      eventSink: new ColumnarEvents(),
      cpuSampleSink: new ColumnarCpuSamples(),
      spanEventSink,
    } as never);
    // load.ts attaches the span store post-parse; mirror that here.
    (col as { spanEvents?: ColumnarSpanEvents }).spanEvents = spanEventSink;

    // Sanity: the columnar branch actually fired, and span events split out.
    expect(col.events).toBeInstanceOf(ColumnarEvents);
    expect(spanEventSink.length).toBeGreaterThan(0);
    expect(col.customEvents.length).toBeLessThan(fat.customEvents.length);

    const wids = deriveWorkerIds(fat);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const fatWS: any = sharedWorkerSpans(fat).workerSpans;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const colWS: any = sharedWorkerSpans(col).workerSpans;

    let checkedPolls = 0, checkedSamples = 0;
    for (const w of wids) {
      const fl = fatWS[w], cl = colWS[w];
      // The columnar lane carries the render dispatch hook.
      expect(cl.store, `w${w} store hook`).toBeDefined();
      expect(cl.w).toBe(w);
      expect(cl.polls.length).toBe(fl.polls.length);
      expect(cl.parks.length).toBe(fl.parks.length);
      expect(cl.actives.length).toBe(fl.actives.length);
      expect(cl.cpuSampleTimes).toEqual(fl.cpuSampleTimes);

      const n = fl.polls.length;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const ts = (a: any[] | undefined) => (a ?? []).map((s) => s.timestamp);
      // Spot indices PLUS the first few polls that actually carry samples (they
      // are sparse across millions of polls, so fixed indices would miss them).
      const idxs = new Set<number>([0, n >> 1, n - 1].filter((x) => x >= 0 && x < n));
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      for (let i = 0, hit = 0; i < n && hit < 5; i++) {
        if (fl.polls[i].cpuSamples || fl.polls[i].schedSamples) { idxs.add(i); hit++; }
      }
      for (const i of idxs) {
        const c = cl.polls.at(i)!, f = fl.polls[i];
        expect([c.start, c.end, c.taskId, c.spawnLoc, !!c.openEnded])
          .toEqual([f.start, f.end, f.taskId, f.spawnLoc ?? null, !!f.openEnded]);
        // cpuSamples/schedSamples reconstructed via CSR match by timestamp.
        expect(ts(c.cpuSamples), `w${w} poll${i} cpu`).toEqual(ts(f.cpuSamples));
        expect(ts(c.schedSamples), `w${w} poll${i} sched`).toEqual(ts(f.schedSamples));
        if (c.cpuSamples || c.schedSamples) checkedSamples++;
        checkedPolls++;
      }
    }
    expect(checkedPolls).toBeGreaterThan(0);
    expect(checkedSamples).toBeGreaterThan(0);

    // Span data: columnar store equals fat allSpans (taskIds, reconstructed
    // segments) - the columnar path carries spans in columnarSpans, not allSpans.
    const fatSD = sharedSpanData(fat);
    const colSD = sharedSpanData(col);
    const cs = colSD.columnarSpans!;
    expect(cs).toBeDefined();
    expect(colSD.allSpans.length).toBe(0);
    expect(cs.length).toBe(fatSD.allSpans.length);
    expect(cs.length).toBeGreaterThan(0);
    for (let i = 0; i < fatSD.allSpans.length; i += Math.max(1, Math.floor(fatSD.allSpans.length / 500))) {
      const a = fatSD.allSpans[i]!, c = cs.at(i);
      expect(c.spanId).toBe(a.spanId);
      expect(c.taskId, `span ${i} taskId`).toBe(a.taskId);
      expect(c.activeNs, `span ${i} activeNs`).toBe(a.activeNs);
      expect(c.segments, `span ${i} segments`).toEqual(a.segments);
    }
    // Lazy id-map adapter materializes the right span (deriveLaneData path).
    const someId = fatSD.allSpans[0]!.spanId;
    expect(cs.spanIdToRow.get(someId)).toBe(0);
  });
});
