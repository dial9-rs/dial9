// Verify isDateLayer() recognizes when a bucket's root children are date
// partitions (YYYY-MM-DD/) rather than genuine key prefixes.
//
// Regression test for issue #471: buckets with no key prefix expose date
// partitions directly at the listing root. Those dates must NOT be treated as
// selectable prefixes - the prefix is empty and the trace data starts at the
// date layer.
//
// Migrated from test_prefix_detection.js (T10); frozen core loaded via
// createRequire (see format.test.ts for the rationale).

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

const { isDateLayer, preferredPrefix } = require("../../prefix_detect.js") as {
  isDateLayer: (children: string[]) => boolean;
  preferredPrefix: (children: string[]) => string | undefined;
};

describe("isDateLayer", () => {
  // Root children that are all dates -> this is a date layer (no prefix).
  it("all date partitions -> date layer", () => {
    expect(isDateLayer(["2026-06-11/", "2026-06-12/"])).toBe(true);
  });

  // A single date partition is still a date layer (auto-select must not fire).
  it("single date partition -> date layer", () => {
    expect(isDateLayer(["2026-06-12/"])).toBe(true);
  });

  // Trailing slash optional.
  it("date without trailing slash -> date layer", () => {
    expect(isDateLayer(["2026-06-12"])).toBe(true);
  });

  // Genuine key prefixes (service names) are NOT a date layer.
  it("service-name prefixes -> not a date layer", () => {
    expect(isDateLayer(["traces/", "checkout-api/"])).toBe(false);
  });

  // A single real prefix is not a date layer.
  it("single real prefix -> not a date layer", () => {
    expect(isDateLayer(["dial9-traces/"])).toBe(false);
  });

  // Mixed dates + real prefix, dates NOT a majority -> not a clean date layer
  // (be conservative, keep offering suggestions rather than silently emptying
  // the prefix).
  it("50/50 dates and prefix -> not a date layer", () => {
    expect(isDateLayer(["2026-06-12/", "traces/"])).toBe(false);
  });

  // Regression (#656) for the gamma bucket that broke browse: many date
  // partitions plus dial9's own auxiliary sibling folder (`diagnostics/`,
  // written by crash capture; `flamegraph-data/` from on-demand aggregation
  // behaves the same). Dates are a strict majority, so this IS a date layer and
  // the prefix must be emptied. Before the majority fix, the lone
  // `diagnostics/` made `.every()` return false, the dates were offered as
  // prefix suggestions, and searching under a `YYYY-MM-DD/` prefix returned
  // zero objects - an empty browse page.
  it("many dates + one auxiliary sibling -> date layer", () => {
    expect(
      isDateLayer(["2026-06-11/", "2026-06-12/", "2026-06-13/", "diagnostics/"]),
    ).toBe(true);
  });

  it("many dates + flamegraph-data sibling -> date layer", () => {
    expect(
      isDateLayer([
        "2026-06-11/",
        "2026-06-12/",
        "2026-06-13/",
        "flamegraph-data/",
      ]),
    ).toBe(true);
  });

  // Empty input -> not a date layer.
  it("empty list -> not a date layer", () => {
    expect(isDateLayer([])).toBe(false);
  });

  // Things that merely start with digits but aren't dates.
  it("partial date-like segments -> not a date layer", () => {
    expect(isDateLayer(["2026/", "2026-06/"])).toBe(false);
  });
});

describe("preferredPrefix", () => {
  // Empty / missing listings have nothing to pre-select.
  it("empty list -> undefined", () => {
    expect(preferredPrefix([])).toBeUndefined();
    expect(preferredPrefix(undefined as unknown as string[])).toBeUndefined();
  });

  // A single prefix is auto-selected (trailing slash stripped).
  it("single prefix -> that prefix", () => {
    expect(preferredPrefix(["traces/"])).toBe("traces");
    expect(preferredPrefix(["checkout-api"])).toBe("checkout-api");
  });

  // With several prefixes and no dial9-traces among them, stay neutral.
  it("multiple non-default prefixes -> undefined", () => {
    expect(preferredPrefix(["traces/", "checkout-api/"])).toBeUndefined();
  });

  // dial9-traces wins whenever it's one of the offered prefixes.
  it("dial9-traces present among many -> dial9-traces", () => {
    expect(
      preferredPrefix(["checkout-api/", "dial9-traces/", "other/"]),
    ).toBe("dial9-traces");
  });

  // Even as the sole prefix, dial9-traces comes back without its slash.
  it("dial9-traces alone -> dial9-traces", () => {
    expect(preferredPrefix(["dial9-traces/"])).toBe("dial9-traces");
  });
});
