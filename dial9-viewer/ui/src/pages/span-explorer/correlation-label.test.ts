import { describe, expect, it } from "vitest";
import { correlationLabel } from "./correlation-label.js";

describe("correlation labels", () => {
  it.each([
    [1, "Perfectly correlated"],
    [-1, "Perfectly inversely correlated"],
    [.999, "Highly correlated"],
    [-.8, "Highly inversely correlated"],
    [.7, "Highly correlated"],
    [.3, "Moderately correlated"],
    [-.2, "Weakly inversely correlated"],
    [.1, "Weakly correlated"],
    [.09, "Not correlated"],
    [0, "Not correlated"],
    [null, "Correlation unavailable"],
    [NaN, "Correlation unavailable"],
  ])("describes %s without presenting a raw coefficient", (coefficient, expected) => {
    expect(correlationLabel(coefficient)).toBe(expected);
  });
});
