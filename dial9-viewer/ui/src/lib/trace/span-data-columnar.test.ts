// buildSpanDataColumnar (over ColumnarSpanEvents + ColumnarWorkerSpans) must
// produce byte-identical SpanData to the frozen buildSpanData(customEvents,
// fatWorkerSpans) - same spans, taskIds, reconstructed segments, activeNs,
// depth, children. The span events are parsed into columns via spanEventSink.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "./load.js";
import { buildSpanData, buildWorkerSpans } from "./analysis.js";
import { deriveWorkerIds } from "./derived.js";
import { ColumnarWorkerSpans } from "./columnar-worker-spans.js";
import { ColumnarSpanEvents } from "./columnar-span-events.js";
import { buildSpanDataColumnar } from "./span-data-columnar.js";

let raw: Uint8Array;
beforeAll(() => {
  const b = readFileSync(fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)));
  raw = b[0] === 0x1f && b[1] === 0x8b ? new Uint8Array(gunzipSync(b)) : new Uint8Array(b);
});

describe("buildSpanDataColumnar matches frozen buildSpanData(customEvents, workerSpans)", () => {
  it("identical allSpans (taskId + reconstructed segments + activeNs), meta, depth, children", async () => {
    // Fat reference: span events IN customEvents.
    const fatTrace = await parseTraceBuffer(raw);
    const wids = deriveWorkerIds(fatTrace);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const fatWs: any = buildWorkerSpans(fatTrace.events, wids, fatTrace.maxTs ?? 0, fatTrace.blockInPlaceGaps).workerSpans;
    const fat = buildSpanData(fatTrace.customEvents, fatWs);

    // Columnar: span events routed to the sink; worker-span store bridged.
    const spanEvents = new ColumnarSpanEvents();
    const colTrace = await parseTraceBuffer(raw, { spanEventSink: spanEvents } as never);
    const store = ColumnarWorkerSpans.fromWorkerSpans(fatWs);
    const col = buildSpanDataColumnar(spanEvents, store);

    // Sanity: the split happened (span events left the fat array).
    expect(spanEvents.length).toBeGreaterThan(0);
    expect(colTrace.customEvents.length).toBeLessThan(fatTrace.customEvents.length);
    expect(fatTrace.customEvents.length - colTrace.customEvents.length).toBe(spanEvents.length);

    // Columnar path: allSpans is empty, spans live in the store.
    const cs = col.columnarSpans!;
    expect(cs).toBeDefined();
    expect(col.allSpans.length).toBe(0);
    expect(cs.length).toBe(fat.allSpans.length);
    expect(cs.length).toBeGreaterThan(0);
    expect(col.maxDepth).toBe(fat.maxDepth);
    expect(col.unmatchedSpans).toEqual(fat.unmatchedSpans);

    // The store materializes each span byte-identical to the fat span, in the
    // same (start-sorted) row order.
    for (let i = 0; i < fat.allSpans.length; i++) {
      const a = fat.allSpans[i]!, c = cs.at(i);
      expect(c.spanId, `span ${i} id`).toBe(a.spanId);
      expect(c.start).toBe(a.start);
      expect(c.end).toBe(a.end);
      expect(c.spanName, `span ${i} name`).toBe(a.spanName);
      expect(c.parentSpanId, `span ${i} parent`).toBe(a.parentSpanId);
      expect(c.taskId, `span ${i} taskId`).toBe(a.taskId);
      expect(c.activeNs, `span ${i} activeNs`).toBe(a.activeNs);
      expect(c.depth, `span ${i} depth`).toBe(a.depth);
      expect(c.segments, `span ${i} segments`).toEqual(a.segments);
      expect(c.fields, `span ${i} fields`).toEqual(a.fields);
      // spanId -> row index round-trips.
      expect(cs.spanIdToRow.get(a.spanId)).toBe(i);
    }

    expect([...col.childrenByParent.entries()].sort()).toEqual([...fat.childrenByParent.entries()].sort());
  });
});
