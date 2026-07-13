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

const { isDateLayer } = require("../../prefix_detect.js") as {
  isDateLayer: (children: string[]) => boolean;
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

  // Mixed dates + real prefix -> not a clean date layer (be conservative,
  // keep offering suggestions rather than silently emptying the prefix).
  it("mixed dates and prefix -> not a date layer", () => {
    expect(isDateLayer(["2026-06-12/", "traces/"])).toBe(false);
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
