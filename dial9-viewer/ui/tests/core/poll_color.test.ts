// Unit tests for pollHeatmapColor - a continuous, log-scale heatmap mapping
// poll duration (in nanoseconds) to a hex color string.
//
// Used by viewer.html for issue #450 (item 1: poll color heatmap).
//
// Migrated from test_poll_color.js (T10); frozen core loaded via
// createRequire (see format.test.ts for the rationale).

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

const { pollHeatmapColor } = require("../../trace_analysis.js") as {
  pollHeatmapColor: (durationNs: number) => string;
};

const HEX_RE = /^#[0-9a-f]{6}$/i;

function hexToRgb(hex: string): [number, number, number] {
  return [
    parseInt(hex.slice(1, 3), 16),
    parseInt(hex.slice(3, 5), 16),
    parseInt(hex.slice(5, 7), 16),
  ];
}

describe("pollHeatmapColor", () => {
  // 1. Returns valid hex strings for any input duration
  it("returns valid #rrggbb for diverse inputs", () => {
    const inputs = [0, 1, 100, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e12];
    for (const d of inputs) {
      const c = pollHeatmapColor(d);
      expect(c, `pollHeatmapColor(${d}) returned ${JSON.stringify(c)}`).toMatch(
        HEX_RE,
      );
    }
  });

  // 2. Monotonic redness - longer polls should be at least as "red"
  //    (R component never decreases) as shorter polls across the interesting
  //    range. The blue channel doesn't need to be strictly monotonic - the ramp
  //    intentionally passes through cyan (peak blue) on its way from dim navy
  //    to red - but the start and end of the range must clearly differ in the
  //    expected direction.
  it("redness grows monotonically (with plateau at 255) across log-spaced 1µs-1s range", () => {
    const samples: number[] = [];
    // Sample log-spaced from 1µs to 1s
    for (let lg = 3; lg <= 9; lg += 0.5) {
      samples.push(Math.pow(10, lg));
    }
    const colors = samples.map((d) => hexToRgb(pollHeatmapColor(d)));
    const reds = colors.map((c) => c[0]);
    const blues = colors.map((c) => c[2]);
    // Per-step: red never decreases (allow plateau at 255)
    for (let i = 1; i < reds.length; i++) {
      expect(
        reds[i]!,
        `red must not decrease between samples ${i - 1} -> ${i}`,
      ).toBeGreaterThanOrEqual(reds[i - 1]!);
    }
    // Overall trend: end clearly redder and less blue than start
    expect(reds[reds.length - 1]!, "red must grow across range").toBeGreaterThan(
      reds[0]!,
    );
    expect(
      blues[blues.length - 1]!,
      "blue must shrink across range",
    ).toBeLessThan(blues[0]!);
  });

  // 3. Clamps below the floor: very short polls (<=100ns) all map to the same
  //    dim color
  it("durations <=100ns clamp to a single floor color", () => {
    const c0 = pollHeatmapColor(0);
    const c1 = pollHeatmapColor(50);
    const c2 = pollHeatmapColor(100);
    expect(c1).toBe(c0);
    expect(c2).toBe(c1);
  });

  // 4. Clamps above the ceiling: very long polls all map to the same hot color
  it("durations >=1s clamp to a single ceiling color", () => {
    const c1 = pollHeatmapColor(1e9); // 1s
    const c2 = pollHeatmapColor(1e10); // 10s
    const c3 = pollHeatmapColor(1e15); // ridiculous
    expect(c2).toBe(c1);
    expect(c3).toBe(c2);
  });

  // 5. Colors at the canonical anchor points should match the legend swatches
  //    so the legend stays an honest reference point.
  it("anchor points (100ns, 10µs, 100µs, 1ms) match legend swatches exactly", () => {
    // These are the colors the heatmap legend explicitly shows. The function
    // must produce them at exactly these durations.
    const ANCHORS = [
      { ns: 100, color: "#2a5a7a", label: "<=100ns (floor: dim navy)" },
      { ns: 10e3, color: "#4fc3f7", label: "10µs (cyan)" },
      { ns: 100e3, color: "#ff8a65", label: "100µs (orange)" },
      { ns: 1e6, color: "#ff4444", label: "1ms (bright red)" },
    ];
    for (const a of ANCHORS) {
      expect(pollHeatmapColor(a.ns).toLowerCase(), a.label).toBe(
        a.color.toLowerCase(),
      );
    }
  });
});
