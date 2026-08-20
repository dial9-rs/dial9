import { describe, expect, it } from "vitest";
import {
  buildBrowseUrl,
  buildServicesUrl,
  canSelectService,
  resolveRequestedService,
  resolveServiceSelection,
} from "./browse-query.js";

describe("buildBrowseUrl", () => {
  it("appends encoded prefix and service filters", () => {
    expect(
      buildBrowseUrl({
        bucket: "trace bucket",
        from: 1000,
        to: 2000,
        prefix: "dial9/traces",
        service: "checkout api",
        layoutHint: '{"v":1}',
      }),
    ).toBe(
      "/api/browse?bucket=trace%20bucket&from=1000&to=2000" +
        "&prefix=dial9%2Ftraces&service=checkout%20api" +
        "&layout_hint=%7B%22v%22%3A1%7D",
    );
  });

  it("omits empty optional filters for an explicit all-services browse", () => {
    expect(
      buildBrowseUrl({ bucket: "traces", from: 1000, to: 2000, prefix: "", service: "" }),
    ).toBe("/api/browse?bucket=traces&from=1000&to=2000");
  });
});

describe("buildServicesUrl", () => {
  it("builds a feeler request without browse data", () => {
    expect(
      buildServicesUrl({
        bucket: "trace bucket",
        from: 1000,
        to: 2000,
        prefix: "dial9/traces",
      }),
    ).toBe(
      "/api/services?bucket=trace%20bucket&from=1000&to=2000" +
        "&prefix=dial9%2Ftraces",
    );
  });

  it("omits an empty prefix", () => {
    expect(
      buildServicesUrl({ bucket: "traces", from: 1000, to: 2000, prefix: "" }),
    ).toBe("/api/services?bucket=traces&from=1000&to=2000");
  });
});

describe("resolveServiceSelection", () => {
  it("automatically focuses a sole service", () => {
    expect(resolveServiceSelection(["api"], "")).toEqual({
      active: "api",
      shouldLoad: true,
    });
  });

  it("keeps multiple services lazy until a tab is selected", () => {
    expect(resolveServiceSelection(["api", "worker"], "")).toEqual({
      active: null,
      shouldLoad: false,
    });
  });

  it("restores an available deep-linked service", () => {
    expect(resolveServiceSelection(["api", "worker"], "worker")).toEqual({
      active: "worker",
      shouldLoad: true,
    });
  });

  it("does not focus a stale deep link", () => {
    expect(resolveServiceSelection(["api", "worker"], "missing")).toEqual({
      active: null,
      shouldLoad: false,
    });
  });
});

describe("resolveRequestedService", () => {
  it("bypasses discovery and loads a service already selected by the URL", () => {
    expect(resolveRequestedService("checkout-api")).toEqual({
      active: "checkout-api",
      shouldLoad: true,
    });
  });

  it("requires discovery when no service is selected", () => {
    expect(resolveRequestedService("")).toBeNull();
  });
});

describe("canSelectService", () => {
  it("keeps tab clicks constrained to discovered services", () => {
    expect(canSelectService(["api"], "worker", false)).toBe(false);
    expect(canSelectService(["api"], "api", false)).toBe(true);
  });

  it("allows browser history to directly load a different URL service", () => {
    expect(canSelectService(["api"], "worker", true)).toBe(true);
  });
});
