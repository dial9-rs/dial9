// The `focus_*` exemplar deep link: parsing, the wall-clock -> monotonic
// conversion, span matching, and the framing.

import { describe, it, expect } from "vitest";
import {
  focusViewport,
  focusWindow,
  matchFocusSpan,
  readFocusLink,
  type FocusCandidate,
} from "./focus-link.js";

function span(
  over: Pick<FocusCandidate, "spanId" | "start" | "end"> & Partial<FocusCandidate>,
): FocusCandidate {
  return { spanName: "op", taskId: null, ...over };
}

describe("readFocusLink", () => {
  it("returns null without focus_start - the anchor the pan needs", () => {
    expect(readFocusLink("?focus_end=200&focus_span_name=op")).toBeNull();
    expect(readFocusLink("")).toBeNull();
  });
  it("reads the full anchor", () => {
    expect(
      readFocusLink("?focus_start=100&focus_end=200&focus_span_name=op&focus_worker=3&focus_task=42"),
    ).toEqual({ startNs: 100, endNs: 200, spanName: "op", workerId: 3, taskId: 42 });
  });
  it("start alone is a valid link", () => {
    expect(readFocusLink("?focus_start=100")).toEqual({ startNs: 100 });
  });
  it("ignores non-numeric and empty values", () => {
    expect(readFocusLink("?focus_start=abc")).toBeNull();
    expect(readFocusLink("?focus_start=100&focus_span_name=")).toEqual({ startNs: 100 });
  });
});

describe("focusWindow", () => {
  // focus_* values are wall-clock; client spans are monotonic.
  it("subtracts the clock offset from both ends", () => {
    expect(focusWindow({ startNs: 1_000, endNs: 3_000 }, 400)).toEqual({ start: 600, end: 2_600 });
  });
  it("a missing end collapses the window onto the start", () => {
    expect(focusWindow({ startNs: 1_000 }, 0)).toEqual({ start: 1_000, end: 1_000 });
  });
  // A trace with no clock anchor must still pan, matching the plain path.
  it("a null offset is treated as zero rather than dropping the link", () => {
    expect(focusWindow({ startNs: 1_000, endNs: 2_000 }, null)).toEqual({
      start: 1_000,
      end: 2_000,
    });
  });
});

describe("matchFocusSpan", () => {
  const spans = [
    span({ spanId: "outer", start: 0, end: 10_000, spanName: "outer" }),
    span({ spanId: "exact", start: 1_000, end: 2_000, spanName: "op" }),
    span({ spanId: "near", start: 1_100, end: 2_400, spanName: "op" }),
    span({ spanId: "elsewhere", start: 50_000, end: 60_000, spanName: "op" }),
  ];

  it("picks the boundary-aligned span, not the enclosing one", () => {
    const hit = matchFocusSpan(spans, { startNs: 1_000, endNs: 2_000, spanName: "op" }, 0);
    expect(hit?.spanId).toBe("exact");
  });

  // Without the name filter a long enclosing span can still overlap; the edge
  // distance is what keeps the tight match winning.
  it("scores by edge distance when no name is given", () => {
    const hit = matchFocusSpan(spans, { startNs: 1_000, endNs: 2_000 }, 0);
    expect(hit?.spanId).toBe("exact");
  });

  it("the name filter excludes non-matching spans entirely", () => {
    const hit = matchFocusSpan(spans, { startNs: 0, endNs: 10_000, spanName: "outer" }, 0);
    expect(hit?.spanId).toBe("outer");
  });

  it("requires temporal overlap", () => {
    expect(matchFocusSpan(spans, { startNs: 30_000, endNs: 31_000, spanName: "op" }, 0)).toBeNull();
  });

  it("applies the clock offset before matching", () => {
    // Wall-clock 6000..7000 with a 5000 offset is monotonic 1000..2000.
    const hit = matchFocusSpan(spans, { startNs: 6_000, endNs: 7_000, spanName: "op" }, 5_000);
    expect(hit?.spanId).toBe("exact");
  });

  it("no candidates yields null", () => {
    expect(matchFocusSpan([], { startNs: 1_000 }, 0)).toBeNull();
  });
});

describe("focusViewport", () => {
  it("frames roughly 5x the span duration with the span past the lead-in", () => {
    const v = focusViewport({ start: 10_000_000, end: 12_000_000 }, {
      minTs: 0,
      maxTs: 100_000_000,
    });
    expect(v.viewEnd - v.viewStart).toBe(10_000_000);
    expect(v.viewStart).toBeLessThan(10_000_000);
    expect(v.viewEnd).toBeGreaterThan(12_000_000);
  });

  it("a zero-length span still gets a visible window", () => {
    const v = focusViewport({ start: 5_000_000, end: 5_000_000 }, { minTs: 0, maxTs: 100_000_000 });
    expect(v.viewEnd - v.viewStart).toBe(1e6);
  });

  it("clamps to the trace bounds", () => {
    const v = focusViewport({ start: 100, end: 200 }, { minTs: 0, maxTs: 500 });
    expect(v.viewStart).toBeGreaterThanOrEqual(0);
    expect(v.viewEnd).toBeLessThanOrEqual(500);
  });
});
