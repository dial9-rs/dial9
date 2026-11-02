import { describe, expect, it } from "vitest";
import type { TailSignal } from "../../lib/trace/span-tail-analysis.js";
import { tailDifferenceLabel } from "./tail-difference-label.js";

const signal = (lowerBound: number | null, count = 100): TailSignal => ({
  key: "memory", unit: "bytes", typical: 1, slow: 2, typicalCount: count, slowCount: count,
  typicalCoverage: .001, slowCoverage: .002, separation: .5, correlation: 0, correlationCount: 100,
  duplicatesLatency: false, filesCompared: 1, filesAgree: 1, points: [],
  tailDifference: lowerBound == null ? null : { cdfGap: .8, samplingMargin: .1, lowerBound,
    comparisons: 10, samplingBudget: .01, typicalMedian: 1, slowMedian: 2, typicalP99: 3, slowP99: 4 },
});

describe("tail difference labels", () => {
  it.each([
    [0, "No clear"], [.001, "Small"], [.099, "Small"], [.1, "Moderate"], [.299, "Moderate"], [.3, "Strong"], [1, "Strong"],
  ] as const)("describes the returned conservative gap %s independently of correlation/coverage", (gap, adjective) => {
    expect(tailDifferenceLabel(signal(gap))).toBe(`${adjective} difference in slow spans`);
  });
  it("distinguishes missing cached evidence and inadequate recorded observations", () => {
    expect(tailDifferenceLabel(signal(null))).toBe("Tail comparison unavailable");
    expect(tailDifferenceLabel(signal(NaN))).toBe("Tail comparison unavailable");
    expect(tailDifferenceLabel(signal(.9, 19))).toBe("Too few values to compare");
  });
  it("uses selected for a custom band", () => {
    expect(tailDifferenceLabel(signal(.5), true)).toBe("Strong difference in selected spans");
  });
});
