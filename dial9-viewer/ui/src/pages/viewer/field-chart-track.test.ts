import { describe, expect, it } from "vitest";
import type { FieldChartSample, FieldChartSeries } from "./field-chart-model.js";
import { buildFieldChartPlot } from "./field-chart-track.js";

function series(samples: readonly FieldChartSample[]): FieldChartSeries {
  const numeric = samples.filter((sample) => sample.value !== null);
  return {
    samples,
    unit: null,
    numericSampleCount: numeric.length,
  };
}

const sample = (
  timestamp: number,
  value: number | bigint | null,
  gap: FieldChartSample["gap"] = null,
): FieldChartSample => ({ timestamp, value, gap });

describe("buildFieldChartPlot", () => {
  it("draws a gauge as a linear path", () => {
    const plot = buildFieldChartPlot(
      series([sample(0, 0n), sample(5, 10n)]),
      "gauge",
      0,
      10,
      100,
      0,
      100,
    );

    expect(plot.segments).toEqual([
      [
        { x: 0, y: 100 },
        { x: 50, y: 0 },
      ],
    ]);
  });

  it("keeps gauge neighbors outside the viewport for true edge interpolation", () => {
    const plot = buildFieldChartPlot(
      series([sample(0, 0n), sample(10, 10n)]),
      "gauge",
      2,
      8,
      60,
      0,
      100,
    );

    expect(plot.segments[0]?.map(({ x }) => x)).toEqual([-20, 80]);
  });

  it("draws counters step-after and breaks the path at resets", () => {
    const plot = buildFieldChartPlot(
      series([
        sample(0, 100n),
        sample(4, 120n),
        sample(5, null, "reset"),
        sample(5, 3n),
        sample(9, 8n),
      ]),
      "counter",
      0,
      10,
      100,
      0,
      100,
    );

    expect(plot.resetXs).toEqual([50]);
    expect(plot.segments).toHaveLength(2);
    expect(plot.segments[0]?.at(-1)?.x).toBe(50);
    expect(plot.segments[1]?.[0]?.x).toBe(50);
    expect(plot.segments[0]?.at(-1)?.y).not.toBe(
      plot.segments[1]?.[0]?.y,
    );
    expect(plot.segments[0]?.slice(0, 3).map(({ x }) => x)).toEqual([
      0, 40, 40,
    ]);
  });

  it("uses a zero baseline and keeps decreases continuous for up/down counters", () => {
    const plot = buildFieldChartPlot(
      series([sample(0, 4), sample(5, -2)]),
      "updown-counter",
      0,
      10,
      100,
      0,
      100,
    );

    expect(plot.segments).toHaveLength(1);
    expect(plot.segments[0]?.map(({ x }) => x)).toEqual([0, 50, 50, 100]);
    expect(plot.baselineY).toBeGreaterThan(0);
    expect(plot.baselineY).toBeLessThan(100);
  });

  it("does not apply a counter sample that occurs after the viewport", () => {
    const plot = buildFieldChartPlot(
      series([sample(0, 0n), sample(5, 5n), sample(15, 10n)]),
      "counter",
      0,
      10,
      100,
      0,
      100,
    );

    expect(plot.max).toBe(5n);
    expect(plot.segments[0]?.map(({ x }) => x)).toEqual([0, 50, 50, 100]);
    expect(plot.segments[0]?.at(-1)?.y).toBe(
      plot.segments[0]?.at(-2)?.y,
    );
  });

  it("keeps adjacent large bigint values visually distinct", () => {
    const plot = buildFieldChartPlot(
      series([
        sample(0, 9_007_199_254_740_993n),
        sample(5, 9_007_199_254_740_994n),
      ]),
      "gauge",
      0,
      10,
      100,
      0,
      100,
    );

    expect(plot.segments[0]?.[0]?.y).not.toBe(
      plot.segments[0]?.[1]?.y,
    );
  });

  it("bounds retained vertices by pixel width while preserving extrema", () => {
    const samples = Array.from({ length: 10_000 }, (_, index) =>
      sample(index / 10, index % 97),
    );
    const plot = buildFieldChartPlot(
      series(samples),
      "counter",
      0,
      1_000,
      20,
      0,
      100,
    );
    const vertices = plot.segments.reduce(
      (count, segment) => count + segment.length,
      0,
    );

    // Four retained points per pixel, expanded to step-after knees, plus the
    // final carry to the viewport edge.
    expect(vertices).toBeLessThanOrEqual(20 * 8 + 1);
    expect(plot.min).toBe(0);
    expect(plot.max).toBe(96);
  });
});
