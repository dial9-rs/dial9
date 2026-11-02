import { describe, expect, it } from "vitest";
import { densityCellSegments, densityColor, numericAxisTick, numericValueAxis, renderNumericPlot } from "./numeric-plot.js";
import type { TailSignal } from "../../lib/trace/span-tail-analysis.js";

const signal = (overrides: Partial<TailSignal> = {}): TailSignal => ({
  key: "work_time", unit: "ns", typical: 1e6, slow: 8e6, typicalCount: 50, slowCount: 20,
  typicalCoverage: 1, slowCoverage: 1, separation: .8, correlation: .8, correlationCount: 100,
  duplicatesLatency: false, filesCompared: 1, filesAgree: 1, points: [],
  ...overrides,
});

function text(value: unknown): string {
  if (Array.isArray(value)) return value.map(text).join("");
  if (value != null && typeof value === "object" && "strings" in value && "values" in value) {
    const template = value as { strings: string[]; values: unknown[] };
    return template.strings.map((part, i) => part + text(template.values[i])).join("");
  }
  return typeof value === "number" || typeof value === "string" ? String(value) : "";
}

describe("backend numeric plot data", () => {
  it("uses backend shared display axes without recalculating them from plotted points", () => {
    const axis = { unit: "ms", multiplier: 1e-6, minimum: 0, maximum: 12 };
    expect(numericValueAxis(signal({ valueAxis: axis }))).toBe(axis);
    expect(numericValueAxis(signal({ unit: null, valueAxis: { ...axis, unit: null, multiplier: 1 } })).unit).toBeNull();
  });

  it("renders full-observation counts even without a navigation sample", () => {
    const field = signal({
      valueAxis: { unit: "ms", multiplier: 1e-6, minimum: 0, maximum: 12 },
      density: { elapsedEdgesMs: [0, 1, 100], valueEdges: [0, 6, 12], counts: [9000, 500, 0, 500], count: 10000, maxCount: 9000 },
    });
    const markup = text(renderNumericPlot(field, { signals: [field], boundaryMs: 10, percentile: 99, minMs: null, maxMs: null, large: false, onJump: () => {} }));
    expect(markup).toContain('data-count=9000');
    expect(markup).toContain("10,000 recorded spans");
    expect(markup).toContain("1–9,000 / cell");
    expect(markup).toContain("log scale");
    expect(markup).not.toContain("No recorded values");
    expect(markup.match(/class="tail-density-cell"/g)).toHaveLength(3);
    expect([...markup.matchAll(/data-count=(\d+)/g)].reduce((sum, match) => sum + Number(match[1]), 0)).toBe(10000);
    expect(markup).toContain('class="tail-density-selected"');
    expect(markup).toContain('pointer-events="none"');
  });

  it("gives zero count no fill and separates sparse/dense counts using the returned maximum", () => {
    expect(densityColor(0, 9000)).toBe("transparent");
    expect(densityColor(1, 9000)).not.toBe(densityColor(9000, 9000));
    expect(densityColor(9000, 9000)).toBe("hsl(194 80% 60%)");
    expect(densityColor(9000, 9000, true)).toBe("hsl(31 80% 60%)");
    expect(densityColor(500, 9000, true).replace("hsl(31", "hsl(194")).toBe(densityColor(500, 9000));
    expect(densityColor(0, 9000, true)).toBe("transparent");
  });

  it("splits a percentile boundary at the exact logarithmic position inside a bin", () => {
    expect(densityCellSegments([0, 1, 100], 1, 10, null)).toEqual([
      { start: 1, end: 1.5, selected: false },
      { start: 1.5, end: 2, selected: true },
    ]);
    expect(densityCellSegments([0, 1, 100], 0, 10, null)).toEqual([
      { start: 0, end: 1, selected: false },
    ]);
  });

  it("splits both endpoints when a custom selection is entirely inside one cell", () => {
    const segments = densityCellSegments([0, 1, 100], 1, 10, Math.sqrt(1000));
    expect(segments.map(segment => segment.selected)).toEqual([false, true, false]);
    expect(segments[0]!.end).toBeCloseTo(1.5);
    expect(segments[1]!.start).toBeCloseTo(1.5);
    expect(segments[1]!.end).toBeCloseTo(1.75);
    expect(segments[2]!.start).toBeCloseTo(1.75);
    expect(segments[2]!.end).toBe(2);
  });

  it("uses a linear split in the zero-inclusive bin and leaves bins outside a selection blue", () => {
    expect(densityCellSegments([0, 1, 100], 0, .25, .75)).toEqual([
      { start: 0, end: .25, selected: false },
      { start: .25, end: .75, selected: true },
      { start: .75, end: 1, selected: false },
    ]);
    expect(densityCellSegments([0, 1, 100], 1, .25, .75)).toEqual([
      { start: 1, end: 2, selected: false },
    ]);
  });

  it("includes only the selected side of a boundary aligned with an existing edge", () => {
    expect(densityCellSegments([0, 1, 100], 0, 1, null)).toEqual([
      { start: 0, end: 1, selected: false },
    ]);
    expect(densityCellSegments([0, 1, 100], 1, 1, null)).toEqual([
      { start: 1, end: 2, selected: true },
    ]);
  });

  it("handles max-only and off-scale selections without coloring outside their interval", () => {
    expect(densityCellSegments([0, 1, 100], 1, 0, 10)).toEqual([
      { start: 1, end: 1.5, selected: true },
      { start: 1.5, end: 2, selected: false },
    ]);
    expect(densityCellSegments([0, 1, 100], 1, 1000, null)).toEqual([
      { start: 1, end: 2, selected: false },
    ]);
    expect(densityCellSegments([0, 1, 100], 1, 10, 5000)).toEqual([
      { start: 1, end: 1.5, selected: false },
      { start: 1.5, end: 2, selected: true },
    ]);
  });

  it("keeps large declared-byte and unknown-unit labels inside the axis gutter", () => {
    expect(numericAxisTick(12e6, "bytes")).toBe("12 MB");
    expect(numericAxisTick(12e3, "bytes")).toBe("12 kB");
    expect(numericAxisTick(12e9, "bytes")).toBe("12 GB");
    expect(numericAxisTick(12e12, "bytes")).toBe("12 TB");
    expect(numericAxisTick(18.446e18, "bytes")).toBe("18.4 EB");
    expect(numericAxisTick(999.999e15, "bytes").length).toBeLessThanOrEqual(8);
    expect(numericAxisTick(12e6, null)).toBe("1.2e7");
    expect(numericAxisTick(-12e6, "count")).toBe("-1.2e7");
  });

  it("describes older reports as samples rather than inferring density", () => {
    const field = signal({
      unit: null,
      points: [[1, 10, { elapsedMs: 1, insideMs: null, startNs: "1", endNs: "1000001", sourceUrl: "demo.bin", requestId: null }]],
    });
    const markup = text(renderNumericPlot(field, { signals: [field], boundaryMs: 10, percentile: 99, minMs: null, maxMs: null, large: false, onJump: () => {} }));
    expect(markup).toContain("unit not recorded");
    expect(markup).toContain("density unavailable in this cached report");
    expect(markup).not.toContain("tail-density-cell");
    expect(markup).not.toContain("tail-density-selected");
  });

  it("labels a custom endpoint beyond loaded elapsed values as off scale", () => {
    const field = signal({
      valueAxis: { unit: "ms", multiplier: 1e-6, minimum: 0, maximum: 12 },
      density: { elapsedEdgesMs: [0, 1, 100], valueEdges: [0, 6, 12], counts: [9000, 500, 0, 500], count: 10000, maxCount: 9000 },
    });
    const markup = text(renderNumericPlot(field, { signals: [field], boundaryMs: 10, percentile: 99, minMs: 10, maxMs: 5000, large: false, onJump: () => {} }));
    expect(markup).toContain("off scale");
  });
});
