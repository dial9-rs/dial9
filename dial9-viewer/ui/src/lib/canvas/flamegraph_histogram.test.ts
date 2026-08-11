// The poll-duration histogram seam re-exports the shared
// flamegraph_histogram.js math used by the canonical flamegraph minimap.

import { describe, it, expect } from "vitest";
import {
  brushToBand,
  fmtDurationNs,
  histogramLayout,
  normalizeHistogram,
  pxToNs,
  sampleWeightedMedianNs,
} from "./flamegraph_histogram.js";

describe("fmtDurationNs", () => {
  it("formats ns/µs/ms/s and rejects negatives", () => {
    expect(fmtDurationNs(500)).toBe("500ns");
    expect(fmtDurationNs(500000)).toBe("500µs");
    expect(fmtDurationNs(1500000)).toBe("1.5ms");
    expect(fmtDurationNs(50000000)).toBe("50ms");
    expect(fmtDurationNs(2000000000)).toBe("2s");
    expect(fmtDurationNs(-1)).toBe("");
  });
});

describe("normalizeHistogram", () => {
  it("sorts ascending and drops malformed entries", () => {
    const norm = normalizeHistogram([
      { lo_ns: 1 << 25, hi_ns: 1 << 26, samples: 1 },
      { lo_ns: 1 << 18, hi_ns: 1 << 19, samples: 2 },
      { lo_ns: 5, hi_ns: 3, samples: 1 }, // hi <= lo
      { lo_ns: 10, hi_ns: 20, samples: -1 }, // negative samples
      null,
    ]);
    expect(norm.length).toBe(2);
    expect(norm[0]!.lo_ns).toBe(1 << 18);
    expect(norm[1]!.lo_ns).toBe(1 << 25);
  });
  it("non-array / empty -> []", () => {
    expect(normalizeHistogram(null)).toEqual([]);
    expect(normalizeHistogram([])).toEqual([]);
  });
});

describe("sampleWeightedMedianNs", () => {
  it("lands in the sample-heavy bucket and follows weight", () => {
    expect(
      sampleWeightedMedianNs([
        { lo_ns: 1 << 18, hi_ns: 1 << 19, samples: 2 },
        { lo_ns: 1 << 25, hi_ns: 1 << 26, samples: 1 },
      ]),
    ).toBe(1 << 18);
    expect(
      sampleWeightedMedianNs([
        { lo_ns: 1 << 18, hi_ns: 1 << 19, samples: 1 },
        { lo_ns: 1 << 25, hi_ns: 1 << 26, samples: 2 },
      ]),
    ).toBe(1 << 25);
    expect(sampleWeightedMedianNs([])).toBeNull();
  });
});

describe("histogramLayout", () => {
  it("equal-width columns with height fraction vs max", () => {
    const { cols, maxSamples } = histogramLayout(
      [
        { lo_ns: 1 << 18, hi_ns: 1 << 19, samples: 2 },
        { lo_ns: 1 << 20, hi_ns: 1 << 21, samples: 8 },
        { lo_ns: 1 << 25, hi_ns: 1 << 26, samples: 4 },
      ],
      300,
      0,
    );
    expect(cols.length).toBe(3);
    expect(maxSamples).toBe(8);
    expect(cols[0]!.x).toBe(0);
    expect(cols[1]!.x).toBe(100);
    expect(cols[0]!.hFrac).toBe(0.25);
    expect(cols[1]!.hFrac).toBe(1);
  });
  it("empty -> no cols", () => {
    expect(histogramLayout([], 300, 1)).toEqual({ cols: [], maxSamples: 0 });
  });
});

describe("brushToBand / pxToNs", () => {
  const bars = [
    { lo_ns: 1 << 18, hi_ns: 1 << 19, samples: 2 },
    { lo_ns: 1 << 20, hi_ns: 1 << 21, samples: 8 },
    { lo_ns: 1 << 25, hi_ns: 1 << 26, samples: 4 },
  ];
  it("maps a brush to continuous outer edges", () => {
    expect(brushToBand(bars, 300, 100, 300)).toEqual({ min_ns: 1 << 20, max_ns: 1 << 26 });
    expect(brushToBand(bars, 300, 0, 300)).toEqual({ min_ns: 1 << 18, max_ns: 1 << 26 });
  });
  it("sub-bucket brush stays inside the bucket, not snapped", () => {
    const band = brushToBand(bars, 300, 25, 75)!;
    expect(band.min_ns).toBeGreaterThan(1 << 18);
    expect(band.min_ns).toBeLessThan(1 << 19);
    expect(band.max_ns).toBeGreaterThan(band.min_ns);
    expect(band.max_ns).toBeLessThan(1 << 19);
  });
  it("a click (near-zero drag) is not a band", () => {
    expect(brushToBand(bars, 300, 150, 150)).toBeNull();
    expect(brushToBand(bars, 300, 150, 151)).toBeNull();
    expect(brushToBand([], 300, 0, 100)).toBeNull();
  });
  it("pxToNs interpolates geometrically within a column", () => {
    const b2 = [
      { lo_ns: 1000, hi_ns: 2000, samples: 1 },
      { lo_ns: 2000, hi_ns: 4000, samples: 1 },
    ];
    expect(pxToNs(b2, 200, 0, null)).toBe(1000);
    expect(pxToNs(b2, 200, 200, null)).toBe(4000);
    expect(pxToNs(b2, 200, 50, null)).toBe(Math.round(1000 * Math.sqrt(2)));
    expect(pxToNs([], 200, 50, null)).toBeNull();
  });
});
