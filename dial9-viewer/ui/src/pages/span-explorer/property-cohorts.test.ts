import { describe, expect, it } from "vitest";
import { propertyCohortCounts } from "./property-cohorts.js";
import { propertyValueColor } from "./categorical-histograms.js";

describe("property cohort count readouts", () => {
  it("preserves cohort denominators and shows missing records explicitly", () => {
    const group = {
      key: "cache_hit", values: ["false", "true"],
      typical: { false: 1 / 7, true: 5 / 7 }, slow: { false: .6, true: .2 },
    };
    expect(propertyCohortCounts(group, "typical", 7)).toEqual({
      values: [{ value: "false", count: 1, share: 1 / 7 }, { value: "true", count: 5, share: 5 / 7 }],
      missing: 1, missingShare: 1 / 7,
    });
    expect(propertyCohortCounts(group, "slow", 5)!.values.map(v => v.count)).toEqual([3, 1]);
    expect(propertyCohortCounts(group, "slow", 5)!.missing).toBe(1);
  });

  it("does not invent proportions when a cohort is empty", () => {
    const group = { key: "cache_hit", values: ["false", "true"], typical: { false: 0, true: 0 }, slow: { false: 0, true: 0 } };
    expect(propertyCohortCounts(group, "slow", 0)!.missingShare).toBe(0);
    expect(propertyCohortCounts(group, "slow", 0)!.values.map(v => v.count)).toEqual([0, 0]);
  });

  it("does not interpret an absent prevalence as zero observations", () => {
    const group = { key: "cache_hit", values: ["false", "true"], typical: { false: 1 }, slow: { false: 0, true: 1 } };
    expect(propertyCohortCounts(group, "typical", 7)).toBeNull();
  });

  it("gives values absent from the selected histogram an explicit muted color", () => {
    expect(propertyValueColor(-1)).toBe("#8393a5");
    expect(propertyValueColor(0)).not.toBe("#8393a5");
  });
});
