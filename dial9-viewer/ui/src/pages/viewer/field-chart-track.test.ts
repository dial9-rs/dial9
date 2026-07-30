import { describe, expect, it } from "vitest";
import type { FieldChartSample, FieldChartSeries } from "./field-chart-model.js";
import {
  buildFieldChartPlot,
  fieldChartHoverAt,
  fieldChartReadoutText,
  fieldChartTooltipRows,
} from "./field-chart-track.js";

function series(samples: readonly FieldChartSample[]): FieldChartSeries {
  return {
    samples,
    unit: null,
  };
}

const point = (
  timestamp: number,
  value: number | bigint | null,
  gap: FieldChartSample["gap"] = null,
): FieldChartSample => ({ timestamp, endTimestamp: null, value, gap });

const interval = (
  timestamp: number,
  endTimestamp: number,
  value: number | bigint,
): FieldChartSample => ({ timestamp, endTimestamp, value, gap: null });

describe("buildFieldChartPlot", () => {
  it("draws a gauge as a linear path", () => {
    const plot = buildFieldChartPlot(
      series([point(0, 0n), point(5, 10n)]),
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
      series([point(0, 0n), point(10, 10n)]),
      "gauge",
      2,
      8,
      60,
      0,
      100,
    );

    expect(plot.segments[0]?.map(({ x }) => x)).toEqual([-20, 80]);
  });

  it("draws counter deltas over their intervals and breaks at resets", () => {
    const plot = buildFieldChartPlot(
      series([
        interval(0, 2, 20n),
        interval(2, 4, 10n),
        point(5, null, "reset"),
        interval(5, 9, 5n),
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
    expect(plot.segments[0]?.at(-1)?.x).toBe(40);
    expect(plot.segments[1]?.[0]?.x).toBe(50);
    expect(plot.segments[0]?.at(-1)?.y).not.toBe(
      plot.segments[1]?.[0]?.y,
    );
    expect(plot.segments[0]?.map(({ x }) => x)).toEqual([0, 20, 20, 40]);
  });

  it("uses a zero baseline for signed up/down deltas", () => {
    const plot = buildFieldChartPlot(
      series([interval(0, 5, -6), interval(5, 10, 3)]),
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
      series([interval(0, 5, 5n), interval(15, 20, 10n)]),
      "counter",
      0,
      10,
      100,
      0,
      100,
    );

    expect(plot.max).toBe(5n);
    expect(plot.segments[0]?.map(({ x }) => x)).toEqual([0, 50]);
  });

  it("keeps adjacent large bigint values visually distinct", () => {
    const plot = buildFieldChartPlot(
      series([
        point(0, 9_007_199_254_740_993n),
        point(5, 9_007_199_254_740_994n),
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
      interval(index / 10, (index + 1) / 10, index % 97),
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

  it("computes average and max from values intersecting the visible window", () => {
    const plot = buildFieldChartPlot(
      series([
        interval(0, 4, 2n),
        interval(4, 8, 6n),
        interval(8, 12, 10n),
        interval(20, 25, 100n),
      ]),
      "counter",
      0,
      10,
      100,
      0,
      100,
    );

    expect(plot.stats).toEqual({
      count: 3,
      sum: 18n,
      min: 2n,
      max: 10n,
    });
    expect(fieldChartReadoutText(plot.stats, null, "counter")).toBe(
      "avg 6 · max 10",
    );
  });

  it("includes min in the up/down summary so negative deltas stay visible", () => {
    expect(
      fieldChartReadoutText(
        { count: 2, sum: 1n, min: -2n, max: 3n },
        null,
        "updown-counter",
      ),
    ).toBe("avg 0.5 · min -2 · max 3");
  });
});

describe("field-chart hover", () => {
  it("selects the nearest gauge observation and stops on an explicit gap", () => {
    const gauge = series([point(0, 1n), point(10, 3n)]);
    expect(fieldChartHoverAt(gauge, "gauge", 6)).toEqual({
      value: 3n,
      timestamp: 10,
      endTimestamp: null,
    });

    const withGap = series([
      point(0, 1n),
      point(5, null, "missing"),
      point(10, 3n),
    ]);
    expect(fieldChartHoverAt(withGap, "gauge", 5)).toBeNull();
  });

  it("resolves the exact counter interval and leaves gaps empty", () => {
    const counter = series([
      interval(0, 5, 2n),
      point(5, null, "reset"),
      interval(5, 10, 3n),
    ]);

    expect(fieldChartHoverAt(counter, "counter", 4.9)).toEqual({
      value: 2n,
      timestamp: 0,
      endTimestamp: 5,
    });
    expect(fieldChartHoverAt(counter, "counter", 5)).toEqual({
      value: 3n,
      timestamp: 5,
      endTimestamp: 10,
    });
    expect(fieldChartHoverAt(counter, "counter", 10)).toBeNull();
  });

  it("describes a counter delta with its interval and duration", () => {
    const spec = {
      id: "field-chart-1",
      eventName: "Metric",
      fieldName: "value",
      kind: "counter",
    } as const;

    expect(
      fieldChartTooltipRows(
        { value: 3n, timestamp: 1_000, endTimestamp: 6_000 },
        spec,
        "widgets",
        (timestamp) => `t${timestamp}`,
      ),
    ).toEqual([
      [{ label: "Series:", value: "Metric.value" }],
      [{ label: "Delta:", value: "3 widgets" }],
      [
        { label: "Interval:", value: "t1000 → t6000" },
        { label: "Duration:", value: "5.0µs" },
      ],
    ]);
  });
});
