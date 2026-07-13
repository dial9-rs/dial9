// Tests for the T14 browser-page format helpers (format.ts), ported from
// the legacy inline helpers. UTC-mode expectations are literal; local-mode
// expectations are round-trip properties (the test host's TZ is arbitrary).

import { describe, expect, it } from "vitest";
import {
  clamp,
  crossesDayBoundary,
  dateToPickerStr,
  fmtTick,
  formatDate,
  formatEpochStr,
  formatSize,
  pickerToDate,
  timeToX,
  xToTime,
} from "./format.js";

describe("formatSize (legacy thresholds)", () => {
  it("bytes / KB / MB bands", () => {
    expect(formatSize(0)).toBe("0 B");
    expect(formatSize(1023)).toBe("1023 B");
    expect(formatSize(1024)).toBe("1.0 KB");
    expect(formatSize(4.1 * 1024 * 1024)).toBe("4.1 MB");
    expect(formatSize(200 * 1024 * 1024)).toBe("200.0 MB");
  });
});

describe("formatDate / formatEpochStr", () => {
  it("UTC mode renders ISO-like without the T", () => {
    expect(formatDate("2026-04-09T19:00:05Z", false)).toBe("2026-04-09 19:00:05");
  });
  it("empty and invalid inputs degrade like legacy", () => {
    expect(formatDate("", false)).toBe("");
    expect(formatDate(undefined, false)).toBe("");
    // An unparseable date makes toISOString throw inside the try, so the
    // raw string comes back (legacy catch branch).
    expect(formatDate("not-a-date", false)).toBe("not-a-date");
  });
  it("epoch 0 renders empty (missing filename epoch, not an error)", () => {
    expect(formatEpochStr(0, false)).toBe("");
    expect(formatEpochStr(1744221600, false)).toBe("2025-04-09 18:00:00");
  });
  it("local mode agrees with the Date accessors", () => {
    const iso = "2026-04-09T19:00:05Z";
    const d = new Date(iso);
    const pad = (n: number) => String(n).padStart(2, "0");
    const expected =
      d.getFullYear() +
      "-" +
      pad(d.getMonth() + 1) +
      "-" +
      pad(d.getDate()) +
      " " +
      pad(d.getHours()) +
      ":" +
      pad(d.getMinutes()) +
      ":" +
      pad(d.getSeconds());
    expect(formatDate(iso, true)).toBe(expected);
  });
});

describe("fmtTick", () => {
  it("HH:MM:SS in UTC mode", () => {
    expect(fmtTick(1744221600, false)).toBe("18:00:00");
  });
  it("withDate prefixes the calendar date (T15 F10-axis amendment)", () => {
    expect(fmtTick(1744221600, false, true)).toBe("2025-04-09 18:00:00");
  });
  it("withDate in local mode matches formatEpochStr", () => {
    expect(fmtTick(1744221600, true, true)).toBe(formatEpochStr(1744221600, true));
  });
});

describe("crossesDayBoundary (T15 F10-axis amendment)", () => {
  // 2025-04-09 00:00:00 UTC.
  const midnight = 1744156800;
  it("false within a single UTC day", () => {
    expect(crossesDayBoundary(midnight + 3600, midnight + 7200, false)).toBe(false);
    // Full day, both ends inside it.
    expect(crossesDayBoundary(midnight, midnight + 86399, false)).toBe(false);
  });
  it("true across a UTC day boundary", () => {
    expect(crossesDayBoundary(midnight + 82800, midnight + 90000, false)).toBe(true);
  });
  it("true across a multi-month span (the dev-seed shape, Finding 3)", () => {
    expect(crossesDayBoundary(1744224000, 1744224000 + 400 * 86400, false)).toBe(true);
  });
  it("local mode compares local calendar dates", () => {
    // One hour on either side of a LOCAL midnight always crosses.
    const d = new Date(2026, 3, 10, 0, 0, 0); // local midnight
    const t = Math.floor(d.getTime() / 1000);
    expect(crossesDayBoundary(t - 3600, t + 3600, true)).toBe(true);
    expect(crossesDayBoundary(t + 3600, t + 7200, true)).toBe(false);
  });
});

describe("picker conversions", () => {
  it("UTC mode round-trips through the picker string", () => {
    const d = new Date("2026-04-09T18:40:00Z");
    const s = dateToPickerStr(d, false);
    expect(s).toBe("2026-04-09T18:40");
    expect(pickerToDate(s, false)!.getTime()).toBe(d.getTime());
  });
  it("local mode round-trips through the picker string", () => {
    const d = new Date(2026, 3, 9, 18, 40); // local wall-clock
    const s = dateToPickerStr(d, true);
    expect(pickerToDate(s, true)!.getTime()).toBe(d.getTime());
  });
  it("empty picker value parses to null", () => {
    expect(pickerToDate("", false)).toBeNull();
    expect(pickerToDate("", true)).toBeNull();
  });
});

describe("geometry helpers", () => {
  it("clamp bounds", () => {
    expect(clamp(5, 0, 10)).toBe(5);
    expect(clamp(-1, 0, 10)).toBe(0);
    expect(clamp(11, 0, 10)).toBe(10);
  });
  it("timeToX and xToTime invert each other", () => {
    const t = xToTime(timeToX(150, 100, 200, 500), 100, 200, 500);
    expect(t).toBeCloseTo(150, 9);
  });
});
