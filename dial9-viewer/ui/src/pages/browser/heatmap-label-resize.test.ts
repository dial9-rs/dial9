import { describe, expect, it } from "vitest";
import {
  clampLabelWidth,
  contentFitLabelWidth,
  MAX_LABEL_WIDTH,
  MIN_LABEL_WIDTH,
} from "./heatmap-label-resize.js";

describe("clampLabelWidth", () => {
  it("keeps the host column within its defined bounds", () => {
    expect(clampLabelWidth(1, 2_000)).toBe(MIN_LABEL_WIDTH);
    expect(clampLabelWidth(2_000, 2_000)).toBe(MAX_LABEL_WIDTH);
  });

  it("leaves enough horizontal space for the heatmap", () => {
    expect(clampLabelWidth(600, 700)).toBe(500);
  });
});

describe("contentFitLabelWidth", () => {
  it("fits the longest label, including row padding", () => {
    expect(contentFitLabelWidth([100, 500], 2_000)).toBe(516);
  });

  it("uses the same minimum and maximum bounds as manual resizing", () => {
    expect(contentFitLabelWidth([10], 2_000)).toBe(MIN_LABEL_WIDTH);
    expect(contentFitLabelWidth([1_000], 2_000)).toBe(MAX_LABEL_WIDTH);
  });
});
