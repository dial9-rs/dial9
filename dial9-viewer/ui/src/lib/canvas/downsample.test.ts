// Wiring tests for the frozen-core re-exports: prove the typed seam
// delivers the real implementations (the core's own behavior is covered
// by its Node suites). Each re-export gets one behavioral smoke check.

import { describe, it, expect } from "vitest";
import {
  makeBarCoalescer,
  pixelCoverage,
  pixelDownsampleSpans,
} from "./downsample.js";

describe("frozen-core downsampling re-exports", () => {
  it("pixelDownsampleSpans: one representative per pixel column", () => {
    // 1000 spans across 10 columns -> at most 10 representatives.
    const spans = Array.from({ length: 1000 }, (_, i) => ({
      start: i,
      end: i + 0.5,
    }));
    const reps = pixelDownsampleSpans(
      spans,
      0,
      0,
      1000,
      10,
      (s) => s.end - s.start,
    );
    expect(reps.length).toBeLessThanOrEqual(10);
    // Element type is preserved through the generic.
    expect(reps[0]!.end - reps[0]!.start).toBeCloseTo(0.5);
  });

  it("makeBarCoalescer: adjacent same-color bars merge into one rect", () => {
    const rects: [number, number, string][] = [];
    const merge = makeBarCoalescer((left, width, color) =>
      rects.push([left, width, color]),
    );
    merge.push(0, 2, "#ff4444");
    merge.push(2, 4, "#ff4444"); // touches previous, same color -> merges
    merge.push(6, 8, "#00ff00");
    merge.flush();
    expect(rects).toEqual([
      [0, 4, "#ff4444"],
      [6, 2, "#00ff00"],
    ]);
  });

  it("pixelCoverage: per-column 0..1 histogram", () => {
    const cov = pixelCoverage([{ start: 0, end: 50 }], 0, 0, 100, 10);
    expect(cov).toBeInstanceOf(Float64Array);
    expect(cov.length).toBe(10);
    expect(cov[0]).toBeCloseTo(1); // fully covered column
    expect(cov[9]).toBeCloseTo(0); // uncovered column
  });
});
