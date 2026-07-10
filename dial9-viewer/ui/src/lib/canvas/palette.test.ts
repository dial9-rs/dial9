// Palette utilities: core re-export wiring + the app-level color lore
// ported from viewer.html inline script (pollColor / pollColorDim /
// spanColor-style assignment).

import { describe, it, expect } from "vitest";
import {
  SPAN_COLORS,
  flamegraphColor,
  makeColorAssigner,
  makeColorDimmer,
  pollColor,
  pollHeatmapColor,
  pollHeatmapColorQuantized,
} from "./palette.js";

describe("frozen-core palette re-exports", () => {
  it("poll heatmap colors are #rrggbb strings", () => {
    expect(pollHeatmapColor(50_000)).toMatch(/^#[0-9a-f]{6}$/i);
    expect(pollHeatmapColorQuantized(50_000)).toMatch(/^#[0-9a-f]{6}$/i);
  });

  it("quantized variant buckets nearby durations to one string", () => {
    // The point of the quantized palette (issue #450): approximately
    // equal durations share the exact color string so the coalescer can
    // merge their bars.
    expect(pollHeatmapColorQuantized(50_000)).toBe(
      pollHeatmapColorQuantized(50_001),
    );
  });

  it("flamegraphColor is deterministic per frame name", () => {
    expect(flamegraphColor("tokio::runtime")).toBe(
      flamegraphColor("tokio::runtime"),
    );
    expect(flamegraphColor("a")).toMatch(/^hsl\(/);
  });
});

describe("pollColor", () => {
  it("is the quantized heatmap of the duration (viewer.html contract)", () => {
    expect(pollColor(1_000, 51_000)).toBe(pollHeatmapColorQuantized(50_000));
  });
});

describe("makeColorDimmer", () => {
  it("scales each channel by the factor, preserving format", () => {
    const dim = makeColorDimmer(0.5);
    expect(dim("#ff8000")).toBe("#804000"); // round(255*.5)=128, round(128*.5)=64
    expect(dim("#000000")).toBe("#000000");
  });

  it("default factor matches the legacy pollColorDim (~0.4)", () => {
    const dim = makeColorDimmer();
    // round(0xff * 0.4) = 102 = 0x66
    expect(dim("#ffffff")).toBe("#666666");
  });

  it("memoizes: same instance returned per input color", () => {
    const dim = makeColorDimmer();
    const a = dim("#123456");
    expect(dim("#123456")).toBe(a);
  });

  it("rejects malformed colors and out-of-range factors loudly", () => {
    expect(() => makeColorDimmer(1.5)).toThrow(RangeError);
    expect(() => makeColorDimmer(-0.1)).toThrow(RangeError);
    expect(() => makeColorDimmer(Number.NaN)).toThrow(RangeError);
    const dim = makeColorDimmer();
    expect(() => dim("red")).toThrow(/#rrggbb/);
    expect(() => dim("#fff")).toThrow(/#rrggbb/);
  });
});

describe("makeColorAssigner", () => {
  it("assigns palette colors in first-seen order and keeps them stable", () => {
    const color = makeColorAssigner();
    const a = color("alpha");
    const b = color("beta");
    expect(a).toBe(SPAN_COLORS[0]);
    expect(b).toBe(SPAN_COLORS[1]);
    expect(color("alpha")).toBe(a); // stable across interleaved lookups
  });

  it("wraps past the palette end", () => {
    const color = makeColorAssigner(["#111111", "#222222"]);
    color("a");
    color("b");
    expect(color("c")).toBe("#111111");
  });

  it("independent assigners give independent sequences (spanColor vs ceColor)", () => {
    const spanColor = makeColorAssigner();
    const ceColor = makeColorAssigner();
    spanColor("only-in-spans");
    expect(ceColor("first-event")).toBe(SPAN_COLORS[0]);
  });

  it("rejects an empty palette", () => {
    expect(() => makeColorAssigner([])).toThrow(RangeError);
  });
});
