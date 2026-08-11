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
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { runInNewContext } from "node:vm";

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

// NOTE: isCoverageFrozen and shouldAutoStopRefining used to live here but were
// dropped from flamegraph_api.js when aggregation became a server-driven SSE
// stream (the server owns the refine/stop loop). isCoverageFrozen's typed port
// still lives in src/lib/trace/aggregates.ts and is covered by
// src/lib/trace/aggregates.test.ts.
const {
  formatCoverageBadge,
  coveragePercent,
  foldErrorNotice,
  nextMaxFiles,
  refinementWorkDepth,
  shouldAdoptRefinementSnapshot,
  nsToPickerUtc,
  pickerUtcToNs,
  msToNs,
  nsToMs,
  sourceFacetOptions,
  threadFacetOptions,
  hostFacetOptions,
} = require("../../flamegraph_api.js") as {
  formatCoverageBadge: (c: Coverage) => string;
  coveragePercent: (c: { files_matched: number; files_folded: number } | null) => number;
  foldErrorNotice: (
    c:
      | {
          fold_errors?: number;
          fold_error_sample?: string;
          files_matched?: number;
          files_folded?: number;
        }
      | null
      | undefined,
  ) => string | null;
  msToNs: (val: string) => string | null;
  nsToMs: (ns: string | null) => string;
  nextMaxFiles: (folded: number, opts?: { cap?: number; min?: number }) => number;
  refinementWorkDepth: (
    coverage: { files_folded: number; fold_work_cap?: number },
    currentMaxFiles: number | null,
  ) => number;
  shouldAdoptRefinementSnapshot: (
    preserveExisting: boolean,
    baselineFilesFolded: number,
    incomingFilesFolded: number,
  ) => boolean;
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

describe("refinement work depth and snapshot adoption", () => {
  it("grows from the work cap rather than all cached files", () => {
    expect(refinementWorkDepth({ files_folded: 500, fold_work_cap: 100 }, null)).toBe(100);
    expect(
      nextMaxFiles(refinementWorkDepth({ files_folded: 500, fold_work_cap: 100 }, null)),
    ).toBe(400);
  });

  it("falls back through the requested cap and folded coverage", () => {
    expect(refinementWorkDepth({ files_folded: 500 }, 80)).toBe(80);
    expect(refinementWorkDepth({ files_folded: 12 }, null)).toBe(12);
  });

  it("keeps the current tree until a same-scope snapshot reaches its baseline", () => {
    expect(shouldAdoptRefinementSnapshot(false, 80, 1)).toBe(true);
    expect(shouldAdoptRefinementSnapshot(true, 80, 79)).toBe(false);
    expect(shouldAdoptRefinementSnapshot(true, 80, 80)).toBe(true);
    expect(shouldAdoptRefinementSnapshot(true, 80, 96)).toBe(true);
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

describe("foldErrorNotice", () => {
  it("no fold errors -> no notice", () => {
    expect(
      foldErrorNotice({ files_matched: 100, files_folded: 0, fold_errors: 0 }),
    ).toBeNull();
  });
  it("zero fold errors -> null even without a sample", () => {
    expect(foldErrorNotice({ fold_errors: 0 })).toBeNull();
  });
  it("null coverage -> null", () => {
    expect(foldErrorNotice(null)).toBeNull();
  });
  it("missing coverage -> null", () => {
    expect(foldErrorNotice(undefined)).toBeNull();
  });
  it("count + sample message", () => {
    expect(
      foldErrorNotice({ fold_errors: 15, fold_error_sample: "1782-4879.bin.gz: AccessDenied" }),
    ).toBe("⚠ 15 files failed to fold — 1782-4879.bin.gz: AccessDenied");
  });
  it("singular noun for one error", () => {
    expect(foldErrorNotice({ fold_errors: 1, fold_error_sample: "x.bin.gz: boom" })).toBe(
      "⚠ 1 file failed to fold — x.bin.gz: boom",
    );
  });
  it("count without a sample message still renders", () => {
    expect(foldErrorNotice({ fold_errors: 3 })).toBe("⚠ 3 files failed to fold");
  });
});

// Poll-duration band ms<->ns conversion (the query-param boundary).
describe("msToNs / nsToMs", () => {
  // msToNs: human milliseconds -> integer-ns string, null for empty/invalid.
  it("msToNs converts and rejects", () => {
    expect(msToNs("10"), "10ms -> 10,000,000ns").toBe("10000000");
    expect(msToNs("0.5"), "fractional 0.5ms -> 500,000ns").toBe("500000");
    expect(msToNs("1.5"), "1.5ms -> 1,500,000ns").toBe("1500000");
    expect(msToNs("0"), "0 is a real bound, not blank").toBe("0");
    expect(msToNs(""), "empty -> null (no bound)").toBeNull();
    expect(msToNs("   "), "blank -> null (no bound)").toBeNull();
    expect(msToNs("abc"), "non-numeric -> null").toBeNull();
    expect(msToNs("-5"), "negative -> null (rejected)").toBeNull();
  });

  // nsToMs: inverse for seeding the input from a URL ns param.
  it("nsToMs inverts", () => {
    expect(nsToMs("10000000"), "10,000,000ns -> 10 (trailing zeros trimmed)").toBe("10");
    expect(nsToMs("1500000"), "1,500,000ns -> 1.5").toBe("1.5");
    expect(nsToMs(""), "empty -> empty").toBe("");
    expect(nsToMs(null), "null -> empty").toBe("");
  });

  it("ms -> ns -> ms round-trips", () => {
    expect(nsToMs(msToNs("2.5"))).toBe("2.5");
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

describe("flamegraph refinement wiring", () => {
  it("publishes refinement helpers through the browser namespace", () => {
    const browserGlobal: {
      window?: unknown;
      FlamegraphApi?: Record<string, unknown>;
    } = {};
    browserGlobal.window = browserGlobal;
    runInNewContext(
      readFileSync(
        fileURLToPath(new URL("../../flamegraph_api.js", import.meta.url)),
        "utf8",
      ),
      browserGlobal,
    );
    expect(typeof browserGlobal.FlamegraphApi?.["refinementWorkDepth"]).toBe("function");
    expect(typeof browserGlobal.FlamegraphApi?.["nextMaxFiles"]).toBe("function");
  });
});
