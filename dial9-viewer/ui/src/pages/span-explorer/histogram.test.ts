import { describe, expect, it } from "vitest";
import {
  alignHistogramToBaseline,
  complementHistogram,
  histogramLayerOrder,
} from "./histogram.js";

describe("Span Explorer histogram comparison", () => {
  it("uses the baseline buckets and restores filtered-out buckets as zeroes", () => {
    expect(
      alignHistogramToBaseline(
        [
          { lo_ns: 1, hi_ns: 2, count: 10 },
          { lo_ns: 2, hi_ns: 4, count: 20 },
          { lo_ns: 4, hi_ns: 8, count: 30 },
        ],
        [
          { lo_ns: 2, hi_ns: 4, count: 5 },
          { lo_ns: 4, hi_ns: 8, count: 7 },
        ],
      ),
    ).toEqual([
      { lo_ns: 1, hi_ns: 2, count: 0 },
      { lo_ns: 2, hi_ns: 4, count: 5 },
      { lo_ns: 4, hi_ns: 8, count: 7 },
    ]);
  });

  it("ignores comparison buckets outside the baseline domain", () => {
    expect(
      alignHistogramToBaseline(
        [{ lo_ns: 2, hi_ns: 4, count: 20 }],
        [
          { lo_ns: 1, hi_ns: 2, count: 3 },
          { lo_ns: 2, hi_ns: 4, count: 5 },
        ],
      ),
    ).toEqual([{ lo_ns: 2, hi_ns: 4, count: 5 }]);
  });

  it("splits the baseline into filtered and Not(filter) without overlap", () => {
    const baseline = [
      { lo_ns: 1, hi_ns: 2, count: 10 },
      { lo_ns: 2, hi_ns: 4, count: 20 },
      { lo_ns: 4, hi_ns: 8, count: 30 },
    ];
    const filtered = [
      { lo_ns: 2, hi_ns: 4, count: 5 },
      { lo_ns: 4, hi_ns: 8, count: 7 },
    ];
    const aligned = alignHistogramToBaseline(baseline, filtered);
    const complement = complementHistogram(baseline, filtered);

    expect(complement).toEqual([
      { lo_ns: 1, hi_ns: 2, count: 10 },
      { lo_ns: 2, hi_ns: 4, count: 15 },
      { lo_ns: 4, hi_ns: 8, count: 23 },
    ]);
    expect(
      baseline.map(
        (bar, index) => aligned[index]!.count + complement[index]!.count,
      ),
    ).toEqual(baseline.map((bar) => bar.count));
  });

  it("paints the shorter population last so it remains visible", () => {
    expect(
      histogramLayerOrder([
        { count: 12, widthScale: 1 },
        { count: 30, widthScale: 0.62 },
      ]),
    ).toEqual([1, 0]);
    expect(
      histogramLayerOrder([
        { count: 40, widthScale: 1 },
        { count: 8, widthScale: 0.62 },
      ]),
    ).toEqual([0, 1]);
  });

  it("paints the narrower bar last when both populations have equal height", () => {
    expect(
      histogramLayerOrder([
        { count: 20, widthScale: 1 },
        { count: 20, widthScale: 0.62 },
      ]),
    ).toEqual([0, 1]);
  });
});
