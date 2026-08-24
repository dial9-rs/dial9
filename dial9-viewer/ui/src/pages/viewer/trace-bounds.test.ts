import { describe, expect, it } from "vitest";
import { traceDisplayBounds } from "./trace-bounds.js";

describe("trace display bounds", () => {
  it("uses all-record bounds when they extend beyond runtime events", () => {
    expect(
      traceDisplayBounds({
        minTs: 100,
        maxTs: 200,
        recordMinTs: 50,
        recordMaxTs: 300,
      }),
    ).toEqual({ minTs: 50, maxTs: 300 });
  });

  it("falls back to runtime bounds for traces without record bounds", () => {
    expect(
      traceDisplayBounds({
        minTs: 100,
        maxTs: 200,
        recordMinTs: null,
        recordMaxTs: null,
      }),
    ).toEqual({ minTs: 100, maxTs: 200 });
  });

  it("makes a single timestamp navigable", () => {
    expect(
      traceDisplayBounds({
        minTs: null,
        maxTs: null,
        recordMinTs: 100,
        recordMaxTs: 100,
      }),
    ).toEqual({ minTs: 100, maxTs: 101 });
  });

  it("rejects absent or inverted bounds", () => {
    expect(
      traceDisplayBounds({
        minTs: null,
        maxTs: null,
        recordMinTs: null,
        recordMaxTs: null,
      }),
    ).toBeNull();
    expect(
      traceDisplayBounds({
        minTs: null,
        maxTs: null,
        recordMinTs: 200,
        recordMaxTs: 100,
      }),
    ).toBeNull();
  });
});
