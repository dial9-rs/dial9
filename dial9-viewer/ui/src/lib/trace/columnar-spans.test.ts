// maxSpanDur + windowRows: the interval bound the viewport window relies on.
// Spans nest (end is NOT monotonic in start order), so the left edge cannot be a
// plain end-column binary search; windowRows must (a) never miss an overlapper
// and (b) scan a bounded band, not the whole prefix.

import { describe, it, expect } from "vitest";
import { ColumnarSpansBuilder } from "./columnar-spans.js";

/** Build a store from [start, end] pairs (spanId = index, no fields/segments). */
function storeOf(spans: readonly [number, number][]) {
  const b = new ColumnarSpansBuilder();
  const parentBySpanId = new Map<string, string | null>();
  for (let i = 0; i < spans.length; i++) {
    const [start, end] = spans[i]!;
    const id = `s${i}`;
    parentBySpanId.set(id, null);
    b.push(start, end, id, "span", null, null, end - start, [], {});
  }
  return b.finish(parentBySpanId).store;
}

/** Brute-force: rows overlapping [viewStart, viewEnd] (end >= vs AND start <= ve). */
function brute(store: ReturnType<typeof storeOf>, vs: number, ve: number): number[] {
  const out: number[] = [];
  for (let r = 0; r < store.length; r++) {
    if (store.end[r]! >= vs && store.start[r]! <= ve) out.push(r);
  }
  return out;
}

describe("ColumnarSpans.maxSpanDur", () => {
  it("is the longest span duration", () => {
    const store = storeOf([[0, 10], [5, 8], [20, 200], [30, 31]]);
    expect(store.maxSpanDur).toBe(180); // 20..200
  });

  it("is 0 for an empty store", () => {
    expect(storeOf([]).maxSpanDur).toBe(0);
  });
});

describe("ColumnarSpans.windowRows", () => {
  // A mix of short and long, nested spans - end is not monotonic in start order.
  const store = storeOf([
    [0, 5],       // r0 short, far left
    [1, 500],     // r1 LONG straddler - its end reaches deep right
    [10, 12],     // r2
    [100, 105],   // r3
    [400, 600],   // r4 long
    [410, 411],   // r5 short inside r4
    [700, 710],   // r6
  ]);

  it("window covers every overlapper (long straddler included)", () => {
    for (const [vs, ve] of [[0, 5], [120, 130], [405, 420], [450, 650], [800, 900]] as const) {
      const { lo, hi } = store.windowRows(vs, ve);
      const found: number[] = [];
      for (let r = lo; r < hi; r++) {
        if (store.end[r]! >= vs && store.start[r]! <= ve) found.push(r);
      }
      expect(found).toEqual(brute(store, vs, ve));
    }
  });

  it("scans a bounded band, not the whole prefix, when panned far right", () => {
    // A right-side view: the window must not start at row 0 just because a long
    // span could straddle. Left bound = start >= viewStart - maxSpanDur.
    const { lo, hi } = store.windowRows(700, 710);
    // floor = 700 - maxSpanDur(599) = 101, so r0..r2 (start < 101) are skipped.
    expect(store.start[lo]).toBeGreaterThanOrEqual(700 - store.maxSpanDur);
    expect(lo).toBeGreaterThan(0);
    expect(hi).toBe(store.length);
  });

  it("empty window when the view is left of all spans", () => {
    const { lo, hi } = store.windowRows(-100, -50);
    expect(hi).toBe(0);
    expect(lo).toBe(0);
  });
});
