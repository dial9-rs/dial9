import { describe, expect, it } from "vitest";
import {
  categoryBinCount,
  categoricalHistogramStack,
  histogramBrushRange,
  histogramMarkerRows,
  histogramPositionToLatency,
  latencyToHistogramPosition,
} from "./categorical-histogram-scale.js";
import type { CategoricalHistogram } from "../../lib/trace/span-tail-analysis.js";

const edges = [0, .01, .1, 1, 10, 100];

describe("Property histogram positions and selections", () => {
  it("keeps zero measurable and interpolates the zero-inclusive bucket linearly", () => {
    expect(latencyToHistogramPosition(edges, 0)).toBe(0);
    expect(latencyToHistogramPosition(edges, .005)).toBe(.5);
    expect(histogramPositionToLatency(edges, .5)).toBe(.005);
  });

  it("places p99 and brush boundaries on the same logarithmic columns", () => {
    const duration = Math.sqrt(10);
    expect(latencyToHistogramPosition(edges, duration)).toBeCloseTo(3.5);
    expect(histogramPositionToLatency(edges, 3.5)).toBeCloseTo(duration);
    expect(histogramBrushRange(edges, 3.5, 4)).toEqual({ minMs: duration, maxMs: 10 });
  });

  it("round-trips duration positions across sub-nanosecond and wide log domains", () => {
    for (const domain of [[0, 1e-8, 1e-6, .0001, .01], edges, [0, 1, 100, 10_000]]) {
      for (let position = 0; position <= domain.length - 1; position += .125) {
        expect(latencyToHistogramPosition(domain, histogramPositionToLatency(domain, position))).toBeCloseTo(position);
      }
    }
  });

  it("clamps reversed out-of-bounds brushes and represents the entire range", () => {
    expect(histogramBrushRange(edges, 7, -2)).toEqual({ minMs: null, maxMs: null });
    expect(histogramBrushRange(edges, 4, 2)).toEqual({ minMs: .1, maxMs: 10 });
  });

  it("keeps coincident p99 labels separate without changing their positions", () => {
    expect(histogramMarkerRows([.2, .2, .2, .6])).toEqual([0, 1, 2, 0]);
    expect(histogramMarkerRows([.6, .2, .4])).toEqual([0, 0, 1]);
  });
});

describe("Property histogram raw counts", () => {
  const value = { value: "hit", count: 3, shares: [1 / 3, 2 / 3], medianMs: 1, p99Ms: 2 };

  it("uses exact wire counts, including zero-count bins", () => {
    expect(categoryBinCount({ ...value, binCounts: [0, 3] }, 0)).toBe(0);
    expect(categoryBinCount({ ...value, binCounts: [0, 3] }, 1)).toBe(3);
  });

  it("does not invent exact counts from percentages in cached older reports", () => {
    expect(categoryBinCount(value, 0)).toBeNull();
    expect(categoryBinCount(value, 1)).toBeNull();
  });

  it("does not invent a count for a missing bin", () => {
    expect(categoryBinCount(value, 2)).toBeNull();
  });
});

describe("Property histogram pooled stack", () => {
  const histogram = (): CategoricalHistogram => ({
    key: "Cache",
    count: 100,
    missingCount: 0,
    binEdgesMs: [0, 1, 10],
    maxShare: 1,
    values: [
      { value: "hit", count: 90, binCounts: [45, 45], shares: [.5, .5], medianMs: 1, p99Ms: 9 },
      { value: "miss", count: 10, binCounts: [10, 0], shares: [1, 0], medianMs: .1, p99Ms: .9 },
    ],
  });

  it("weights unequal populations so layers sum to the total distribution", () => {
    const stack = categoricalHistogramStack(histogram());
    expect(stack.available).toBe(true);
    if (!stack.available) throw new Error(stack.reason);
    expect(stack.population).toBe(100);
    expect(stack.totalCounts).toEqual([55, 45]);
    expect(stack.totalShares).toEqual([.55, .45]);
    expect(stack.layers[0]).toEqual({ lower: [0, 0], upper: [.45, .45] });
    expect(stack.layers[1]).toEqual({ lower: [.45, .45], upper: [.55, .45] });
    expect(stack.totalShares.reduce((sum, share) => sum + share, 0)).toBe(1);
    expect(stack.maximum).toBe(.55);
  });

  it("excludes missing values from the explicitly recorded population", () => {
    const stack = categoricalHistogramStack({ ...histogram(), missingCount: 500 });
    expect(stack.available).toBe(true);
    if (!stack.available) throw new Error(stack.reason);
    expect(stack.population).toBe(100);
    expect(stack.totalShares).toEqual([.55, .45]);
  });

  it("does not fabricate an Other distribution for omitted values", () => {
    expect(categoricalHistogramStack({ ...histogram(), count: 120 })).toEqual({
      available: false, reason: "incomplete-values",
    });
  });

  it("rejects populations that exceed the reported total", () => {
    expect(categoricalHistogramStack({ ...histogram(), count: 99 })).toEqual({
      available: false, reason: "invalid-counts",
    });
  });

  it("requires exact bin counts in cached older reports", () => {
    const legacy = histogram();
    delete legacy.values[0]!.binCounts;
    expect(categoricalHistogramStack(legacy)).toEqual({
      available: false, reason: "missing-counts",
    });
  });

  it.each([
    ["missing bin", [90]],
    ["negative bin", [-1, 91]],
    ["fractional bin", [45.5, 44.5]],
    ["wrong total", [45, 44]],
  ])("rejects %s instead of drawing false weights", (_name, counts) => {
    const invalid = histogram();
    invalid.values[0]!.binCounts = counts as number[];
    expect(categoricalHistogramStack(invalid)).toEqual({
      available: false, reason: "invalid-counts",
    });
  });
});
