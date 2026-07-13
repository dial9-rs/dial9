// Unit tests for the tokio-stats formatters (features/04 B2/B3/C3). Pure
// ports of the legacy converters; the UTC direction is a parameter here.

import { describe, it, expect } from "vitest";
import {
  datetimeToNs,
  formatDuration,
  nsToDatetime,
  thresholdNs,
} from "./format.js";

describe("nsToDatetime / datetimeToNs", () => {
  it("empty/absent bounds render blank; blank parses to null", () => {
    expect(nsToDatetime(null, true)).toBe("");
    expect(nsToDatetime("", true)).toBe("");
    expect(datetimeToNs("", true)).toBeNull();
    expect(datetimeToNs("", false)).toBeNull();
  });

  it("UTC mode formats the ns as the UTC wall-clock ISO slice", () => {
    const ns = String(Date.UTC(2026, 3, 9, 19, 0, 7) * 1e6);
    expect(nsToDatetime(ns, true)).toBe("2026-04-09T19:00:07");
  });

  it("UTC round-trips picker <-> ns exactly (ms floor)", () => {
    const ns = datetimeToNs("2026-04-09T19:00:07", true);
    expect(ns).toBe(String(Date.UTC(2026, 3, 9, 19, 0, 7) * 1e6));
    expect(nsToDatetime(ns, true)).toBe("2026-04-09T19:00:07");
  });

  it("local mode round-trips regardless of the host timezone", () => {
    // The read (ns->picker) and write (picker->ns) sides are symmetric in
    // both modes, so a value entered locally comes back identical.
    const ns = datetimeToNs("2026-04-09T19:00:07", false);
    expect(nsToDatetime(ns, false)).toBe("2026-04-09T19:00:07");
  });
});

describe("formatDuration (unit ladder s/ms/us/ns)", () => {
  it("seconds and milliseconds carry two decimals", () => {
    expect(formatDuration(1e9)).toBe("1.00s");
    expect(formatDuration(1.5e9)).toBe("1.50s");
    expect(formatDuration(1e6)).toBe("1.00ms");
    expect(formatDuration(1.234e6)).toBe("1.23ms");
  });
  it("microseconds are whole numbers with the U+00B5 micro sign", () => {
    expect(formatDuration(500000)).toBe("500µs");
    expect(formatDuration(100000)).toBe("100µs");
  });
  it("sub-microsecond durations render as raw ns", () => {
    expect(formatDuration(999)).toBe("999ns");
    expect(formatDuration(0)).toBe("0ns");
  });
});

describe("thresholdNs (log-scale slider)", () => {
  it("maps the slider range to 100us..1s in ns, default 1ms", () => {
    expect(thresholdNs("0")).toBe(1e6); // default -> 1ms
    expect(thresholdNs("-1")).toBe(100000); // floor -> 100us (server floor)
    expect(thresholdNs("3")).toBe(1e9); // ceiling -> 1s
  });
});
