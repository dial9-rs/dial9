// Unit tests for the API-mode flamegraph refinement helpers in
// flamegraph_api.js: coverage-badge formatting, freeze detection, and the
// "Fetch more" max_files computation. These are the pure pieces of the
// poll loop in flamegraph.html, factored out so they can be tested without a
// browser DOM.
//
// Migrated from test_flamegraph_api.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale).

// Run the datetime round-trip assertions in a negative-offset timezone so a
// regression to local-time parsing is caught: under UTC the bug is invisible.
// Set at module scope (before any Date use), exactly like the original
// standalone script; Node re-reads TZ on each Date call.
process.env["TZ"] = "America/New_York"; // UTC-4 (DST) / UTC-5

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

interface Coverage {
  files_matched: number;
  files_folded: number;
  samples_folded: number;
  hosts_matched?: number;
  hosts_folded?: number;
}

interface FacetOption {
  value: string;
  label: string;
}

const {
  formatCoverageBadge,
  isCoverageFrozen,
  coveragePercent,
  shouldAutoStopRefining,
  nextMaxFiles,
  nsToPickerUtc,
  pickerUtcToNs,
  sourceFacetOptions,
  threadFacetOptions,
  hostFacetOptions,
} = require("../../flamegraph_api.js") as {
  formatCoverageBadge: (c: Coverage) => string;
  isCoverageFrozen: (
    prev: { files_folded: number } | null,
    cur: { files_folded: number },
  ) => boolean;
  coveragePercent: (c: { files_matched: number; files_folded: number } | null) => number;
  shouldAutoStopRefining: (
    history: number[],
    opts?: { minDeltaPct?: number; patience?: number },
  ) => boolean;
  nextMaxFiles: (folded: number, opts?: { cap?: number; min?: number }) => number;
  nsToPickerUtc: (ns: string | null) => string;
  pickerUtcToNs: (picker: string) => string | null;
  sourceFacetOptions: (present?: string[]) => FacetOption[];
  threadFacetOptions: (present: string[]) => FacetOption[];
  hostFacetOptions: (hosts: string[]) => FacetOption[];
};

describe("formatCoverageBadge", () => {
  it("spec example", () => {
    expect(
      formatCoverageBadge({ files_matched: 480, files_folded: 12, samples_folded: 41203 }),
    ).toBe("12 / 480 files (2.5%) · 41,203 samples");
  });
  it("fully folded", () => {
    expect(
      formatCoverageBadge({ files_matched: 480, files_folded: 480, samples_folded: 1000000 }),
    ).toBe("480 / 480 files (100.0%) · 1,000,000 samples");
  });
  it("zero denominator does not produce NaN%", () => {
    expect(
      formatCoverageBadge({ files_matched: 0, files_folded: 0, samples_folded: 0 }),
    ).toBe("0 / 0 files (0.0%) · 0 samples");
  });
  it("rounds percent to one decimal", () => {
    expect(
      formatCoverageBadge({ files_matched: 3, files_folded: 1, samples_folded: 7 }),
    ).toBe("1 / 3 files (33.3%) · 7 samples");
  });
  it("host spread shown when scope spans multiple hosts", () => {
    expect(
      formatCoverageBadge({
        files_matched: 480,
        files_folded: 12,
        samples_folded: 41203,
        hosts_matched: 40,
        hosts_folded: 8,
      }),
    ).toBe("12 / 480 files (2.5%) · 8 / 40 hosts · 41,203 samples");
  });
  it("single-host scope omits the uninformative host fraction", () => {
    expect(
      formatCoverageBadge({
        files_matched: 5,
        files_folded: 2,
        samples_folded: 99,
        hosts_matched: 1,
        hosts_folded: 1,
      }),
    ).toBe("2 / 5 files (40.0%) · 99 samples");
  });
});

describe("isCoverageFrozen", () => {
  it("first poll (no previous) is never frozen", () => {
    expect(isCoverageFrozen(null, { files_folded: 5 })).toBe(false);
  });
  it("progress (folded increased) is not frozen", () => {
    expect(isCoverageFrozen({ files_folded: 5 }, { files_folded: 8 })).toBe(false);
  });
  it("no increase is frozen", () => {
    expect(isCoverageFrozen({ files_folded: 8 }, { files_folded: 8 })).toBe(true);
  });
  it("decrease is frozen (defensive)", () => {
    expect(isCoverageFrozen({ files_folded: 8 }, { files_folded: 7 })).toBe(true);
  });
});

describe("nextMaxFiles", () => {
  it("4x current fold count", () => {
    expect(nextMaxFiles(12)).toBe(48);
  });
  it("zero folded falls back to min", () => {
    expect(nextMaxFiles(0)).toBe(16);
  });
  it("small fold count clamps up to min", () => {
    expect(nextMaxFiles(2)).toBe(16);
  });
  it("above the min floor uses 4x", () => {
    expect(nextMaxFiles(5)).toBe(20);
  });
  it("caps at default ceiling", () => {
    expect(nextMaxFiles(1_000_000)).toBe(100000);
  });
  it("respects custom cap", () => {
    expect(nextMaxFiles(12, { cap: 30 })).toBe(30);
  });
  it("respects custom min", () => {
    expect(nextMaxFiles(1, { min: 100 })).toBe(100);
  });
});

describe("coveragePercent", () => {
  it("50/200 = 25%", () => {
    expect(coveragePercent({ files_matched: 200, files_folded: 50 })).toBe(25);
  });
  it("zero denom -> 0", () => {
    expect(coveragePercent({ files_matched: 0, files_folded: 0 })).toBe(0);
  });
  it("null coverage -> 0", () => {
    expect(coveragePercent(null)).toBe(0);
  });
});

describe("shouldAutoStopRefining", () => {
  it("no history -> keep refining", () => {
    expect(shouldAutoStopRefining([])).toBe(false);
  });
  it("fewer than patience(3) samples -> keep going", () => {
    expect(shouldAutoStopRefining([5, 4])).toBe(false);
  });
  it("recent gains still large -> keep going", () => {
    expect(shouldAutoStopRefining([5, 4, 3])).toBe(false);
  });
  it("3 consecutive sub-0.5pp gains -> stop", () => {
    expect(shouldAutoStopRefining([0.1, 0.2, 0.05])).toBe(true);
  });
  it("only the most recent `patience` matter (early spike ignored once it settles)", () => {
    expect(shouldAutoStopRefining([5, 0.1, 0.2, 0.3])).toBe(true);
  });
  it("a large gain within the recent window prevents stopping", () => {
    expect(shouldAutoStopRefining([0.1, 0.1, 5, 0.1, 0.2])).toBe(false);
  });
  it("a large most-recent gain prevents stopping", () => {
    expect(shouldAutoStopRefining([0.1, 0.1, 1.0])).toBe(false);
  });
  it("custom thresholds: both gains below 5pp -> stop", () => {
    expect(shouldAutoStopRefining([2, 2], { minDeltaPct: 5, patience: 2 })).toBe(true);
  });
});

describe("nsToPickerUtc / pickerUtcToNs (timezone round-trip)", () => {
  // 1782155999000000000 ns = 2026-06-22 19:19:59 UTC.
  it("ns -> picker shows UTC wall-clock", () => {
    expect(nsToPickerUtc("1782155999000000000")).toBe("2026-06-22T19:19:59");
  });
  it("empty picker -> null", () => {
    expect(pickerUtcToNs("")).toBeNull();
  });
  it("empty ns -> empty string", () => {
    expect(nsToPickerUtc("")).toBe("");
  });
  it("null ns -> empty string", () => {
    expect(nsToPickerUtc(null)).toBe("");
  });

  // The core regression: the value the backend receives must equal the value in
  // the URL, regardless of the viewer's timezone. The pre-fix code parsed the
  // picker string as local time, adding the UTC offset (+4h here) and querying
  // the future.
  it("picker -> ns parses as UTC (not local), so no offset shift", () => {
    expect(pickerUtcToNs("2026-06-22T19:19:59")).toBe("1782155999000000000");
  });

  // Full round-trip is the identity for several instants, in this UTC-4 zone.
  for (const ns of [
    "1782155999000000000", // 2026-06-22 19:19:59 UTC
    "1767225600000000000", // 2026-01-01 00:00:00 UTC (standard time, UTC-5)
    "1781874000000000000", // 2026-06-19 13:00:00 UTC
  ]) {
    it(`round-trip identity for ${ns}`, () => {
      expect(pickerUtcToNs(nsToPickerUtc(ns))).toBe(ns);
    });
  }
});

describe("data-driven facet options", () => {
  // sourceFacetOptions: only present sources, "All" only when >1.
  it("both sources present -> CPU, Sched, All", () => {
    expect(sourceFacetOptions(["cpu", "sched"])).toEqual([
      { value: "cpu", label: "CPU" },
      { value: "sched", label: "Sched" },
      { value: "all", label: "All" },
    ]);
  });
  it("single source present -> no All option", () => {
    expect(sourceFacetOptions(["cpu"])).toEqual([{ value: "cpu", label: "CPU" }]);
  });
  it("only sched present -> Sched only", () => {
    expect(sourceFacetOptions(["sched"])).toEqual([{ value: "sched", label: "Sched" }]);
  });
  it("empty/absent facets fall back to CPU", () => {
    expect(sourceFacetOptions([])).toEqual([{ value: "cpu", label: "CPU" }]);
  });
  it("undefined facets fall back to CPU", () => {
    expect(sourceFacetOptions(undefined)).toEqual([{ value: "cpu", label: "CPU" }]);
  });
  it("source order is canonical (cpu before sched)", () => {
    // Canonical order regardless of input order.
    expect(sourceFacetOptions(["sched", "cpu"])).toEqual([
      { value: "cpu", label: "CPU" },
      { value: "sched", label: "Sched" },
      { value: "all", label: "All" },
    ]);
  });

  // threadFacetOptions: leading All, then only present classes.
  it("both thread classes -> All, Worker, Off-worker", () => {
    expect(threadFacetOptions(["worker", "off-worker"])).toEqual([
      { value: "", label: "All" },
      { value: "worker", label: "Worker" },
      { value: "off-worker", label: "Off-worker" },
    ]);
  });
  it("only worker present -> All, Worker", () => {
    expect(threadFacetOptions(["worker"])).toEqual([
      { value: "", label: "All" },
      { value: "worker", label: "Worker" },
    ]);
  });
  it("empty facets fall back to full thread set", () => {
    expect(threadFacetOptions([])).toEqual([
      { value: "", label: "All" },
      { value: "worker", label: "Worker" },
      { value: "off-worker", label: "Off-worker" },
    ]);
  });

  // hostFacetOptions: leading All (with count when >1), then each host.
  it("multiple hosts -> All (N hosts) + each host", () => {
    expect(hostFacetOptions(["host-a", "host-b", "host-c"])).toEqual([
      { value: "", label: "All (3 hosts)" },
      { value: "host-a", label: "host-a" },
      { value: "host-b", label: "host-b" },
      { value: "host-c", label: "host-c" },
    ]);
  });
  it("single host -> plain All + the host", () => {
    expect(hostFacetOptions(["host-a"])).toEqual([
      { value: "", label: "All" },
      { value: "host-a", label: "host-a" },
    ]);
  });
  it("no hosts -> just All", () => {
    expect(hostFacetOptions([])).toEqual([{ value: "", label: "All" }]);
  });
});
