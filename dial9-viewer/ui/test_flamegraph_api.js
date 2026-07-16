#!/usr/bin/env node
"use strict";

// Unit tests for the API-mode flamegraph refinement helpers in
// flamegraph_api.js: coverage-badge formatting, freeze detection, and the
// "Fetch more" max_files computation. These are the pure pieces of the
// poll loop in flamegraph.html, factored out so they can be tested without a
// browser DOM.

// Run the datetime round-trip assertions in a negative-offset timezone so a
// regression to local-time parsing is caught: under UTC the bug is invisible.
process.env.TZ = "America/New_York"; // UTC-4 (DST) / UTC-5

const {
  formatCoverageBadge,
  foldErrorNotice,
  coveragePercent,
  nextMaxFiles,
  nsToPickerUtc,
  pickerUtcToNs,
  msToNs,
  nsToMs,
  sourceFacetOptions,
  threadFacetOptions,
  hostFacetOptions,
} = require("./flamegraph_api.js");

let passed = 0;
let failed = 0;

function assertEq(actual, expected, desc) {
  if (actual === expected) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(
      `✗ ${desc}\n    expected: ${JSON.stringify(expected)}\n    actual:   ${JSON.stringify(actual)}`,
    );
    failed++;
  }
}

// ── formatCoverageBadge ──
assertEq(
  formatCoverageBadge({ files_matched: 480, files_folded: 12, samples_folded: 41203 }),
  "12 / 480 files (2.5%) · 41,203 samples",
  "spec example",
);
assertEq(
  formatCoverageBadge({ files_matched: 480, files_folded: 480, samples_folded: 1000000 }),
  "480 / 480 files (100.0%) · 1,000,000 samples",
  "fully folded",
);
assertEq(
  formatCoverageBadge({ files_matched: 0, files_folded: 0, samples_folded: 0 }),
  "0 / 0 files (0.0%) · 0 samples",
  "zero denominator does not produce NaN%",
);
assertEq(
  formatCoverageBadge({ files_matched: 3, files_folded: 1, samples_folded: 7 }),
  "1 / 3 files (33.3%) · 7 samples",
  "rounds percent to one decimal",
);
assertEq(
  formatCoverageBadge({
    files_matched: 480,
    files_folded: 12,
    samples_folded: 41203,
    hosts_matched: 40,
    hosts_folded: 8,
  }),
  "12 / 480 files (2.5%) · 8 / 40 hosts · 41,203 samples",
  "host spread shown when scope spans multiple hosts",
);
assertEq(
  formatCoverageBadge({
    files_matched: 5,
    files_folded: 2,
    samples_folded: 99,
    hosts_matched: 1,
    hosts_folded: 1,
  }),
  "2 / 5 files (40.0%) · 99 samples",
  "single-host scope omits the uninformative host fraction",
);

// ── foldErrorNotice ──
assertEq(
  foldErrorNotice({ files_matched: 100, files_folded: 0, fold_errors: 0 }),
  null,
  "no fold errors -> no notice",
);
assertEq(
  foldErrorNotice({ fold_errors: 0 }),
  null,
  "zero fold errors -> null even without a sample",
);
assertEq(foldErrorNotice(null), null, "null coverage -> null");
assertEq(foldErrorNotice(undefined), null, "missing coverage -> null");
assertEq(
  foldErrorNotice({ fold_errors: 15, fold_error_sample: "1782-4879.bin.gz: AccessDenied" }),
  "⚠ 15 files failed to fold — 1782-4879.bin.gz: AccessDenied",
  "count + sample message",
);
assertEq(
  foldErrorNotice({ fold_errors: 1, fold_error_sample: "x.bin.gz: boom" }),
  "⚠ 1 file failed to fold — x.bin.gz: boom",
  "singular noun for one error",
);
assertEq(
  foldErrorNotice({ fold_errors: 3 }),
  "⚠ 3 files failed to fold",
  "count without a sample message still renders",
);

// ── nextMaxFiles ──
assertEq(nextMaxFiles(12), 48, "4x current fold count");
assertEq(nextMaxFiles(0), 16, "zero folded falls back to min");
assertEq(nextMaxFiles(2), 16, "small fold count clamps up to min");
assertEq(nextMaxFiles(5), 20, "above the min floor uses 4x");
assertEq(nextMaxFiles(1_000_000), 100000, "caps at default ceiling");
assertEq(nextMaxFiles(12, { cap: 30 }), 30, "respects custom cap");
assertEq(nextMaxFiles(1, { min: 100 }), 100, "respects custom min");

// ── coveragePercent ──
assertEq(
  coveragePercent({ files_matched: 200, files_folded: 50 }),
  25,
  "50/200 = 25%",
);
assertEq(coveragePercent({ files_matched: 0, files_folded: 0 }), 0, "zero denom -> 0");
assertEq(coveragePercent(null), 0, "null coverage -> 0");

// ── nsToPickerUtc / pickerUtcToNs (timezone round-trip) ──
// 1782155999000000000 ns = 2026-06-22 19:19:59 UTC.
assertEq(
  nsToPickerUtc("1782155999000000000"),
  "2026-06-22T19:19:59",
  "ns -> picker shows UTC wall-clock",
);
assertEq(pickerUtcToNs(""), null, "empty picker -> null");
assertEq(nsToPickerUtc(""), "", "empty ns -> empty string");
assertEq(nsToPickerUtc(null), "", "null ns -> empty string");

// The core regression: the value the backend receives must equal the value in
// the URL, regardless of the viewer's timezone. The pre-fix code parsed the
// picker string as local time, adding the UTC offset (+4h here) and querying
// the future.
assertEq(
  pickerUtcToNs("2026-06-22T19:19:59"),
  "1782155999000000000",
  "picker -> ns parses as UTC (not local), so no offset shift",
);

// Full round-trip is the identity for several instants, in this UTC-4 zone.
for (const ns of [
  "1782155999000000000", // 2026-06-22 19:19:59 UTC
  "1767225600000000000", // 2026-01-01 00:00:00 UTC (standard time, UTC-5)
  "1781874000000000000", // 2026-06-19 13:00:00 UTC
]) {
  assertEq(
    pickerUtcToNs(nsToPickerUtc(ns)),
    ns,
    `round-trip identity for ${ns}`,
  );
}

// ── Data-driven facet options ──
function assertDeepEq(actual, expected, desc) {
  if (JSON.stringify(actual) === JSON.stringify(expected)) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(
      `✗ ${desc}\n    expected: ${JSON.stringify(expected)}\n    actual:   ${JSON.stringify(actual)}`,
    );
    failed++;
  }
}

// sourceFacetOptions: only present sources, "All" only when >1.
assertDeepEq(
  sourceFacetOptions(["cpu", "sched"]),
  [
    { value: "cpu", label: "CPU" },
    { value: "sched", label: "Sched" },
    { value: "all", label: "All" },
  ],
  "both sources present -> CPU, Sched, All",
);
assertDeepEq(
  sourceFacetOptions(["cpu"]),
  [{ value: "cpu", label: "CPU" }],
  "single source present -> no All option",
);
assertDeepEq(
  sourceFacetOptions(["sched"]),
  [{ value: "sched", label: "Sched" }],
  "only sched present -> Sched only",
);
assertDeepEq(
  sourceFacetOptions([]),
  [{ value: "cpu", label: "CPU" }],
  "empty/absent facets fall back to CPU",
);
assertDeepEq(
  sourceFacetOptions(undefined),
  [{ value: "cpu", label: "CPU" }],
  "undefined facets fall back to CPU",
);
// Canonical order regardless of input order.
assertDeepEq(
  sourceFacetOptions(["sched", "cpu"]),
  [
    { value: "cpu", label: "CPU" },
    { value: "sched", label: "Sched" },
    { value: "all", label: "All" },
  ],
  "source order is canonical (cpu before sched)",
);

// threadFacetOptions: leading All, then only present classes.
assertDeepEq(
  threadFacetOptions(["worker", "off-worker"]),
  [
    { value: "", label: "All" },
    { value: "worker", label: "Worker" },
    { value: "off-worker", label: "Off-worker" },
  ],
  "both thread classes -> All, Worker, Off-worker",
);
assertDeepEq(
  threadFacetOptions(["worker"]),
  [
    { value: "", label: "All" },
    { value: "worker", label: "Worker" },
  ],
  "only worker present -> All, Worker",
);
assertDeepEq(
  threadFacetOptions([]),
  [
    { value: "", label: "All" },
    { value: "worker", label: "Worker" },
    { value: "off-worker", label: "Off-worker" },
  ],
  "empty facets fall back to full thread set",
);

// hostFacetOptions: leading All (with count when >1), then each host.
assertDeepEq(
  hostFacetOptions(["host-a", "host-b", "host-c"]),
  [
    { value: "", label: "All (3 hosts)" },
    { value: "host-a", label: "host-a" },
    { value: "host-b", label: "host-b" },
    { value: "host-c", label: "host-c" },
  ],
  "multiple hosts -> All (N hosts) + each host",
);
assertDeepEq(
  hostFacetOptions(["host-a"]),
  [
    { value: "", label: "All" },
    { value: "host-a", label: "host-a" },
  ],
  "single host -> plain All + the host",
);
assertDeepEq(hostFacetOptions([]), [{ value: "", label: "All" }], "no hosts -> just All");

// ── Poll-duration band ms↔ns conversion (the query-param boundary) ──
// msToNs: human milliseconds → integer-ns string, null for empty/invalid.
assertEq(msToNs("10"), "10000000", "msToNs: 10ms -> 10,000,000ns");
assertEq(msToNs("0.5"), "500000", "msToNs: fractional 0.5ms -> 500,000ns");
assertEq(msToNs("1.5"), "1500000", "msToNs: 1.5ms -> 1,500,000ns");
assertEq(msToNs("0"), "0", "msToNs: 0 is a real bound, not blank");
assertEq(msToNs(""), null, "msToNs: empty -> null (no bound)");
assertEq(msToNs("   "), null, "msToNs: blank -> null (no bound)");
assertEq(msToNs("abc"), null, "msToNs: non-numeric -> null");
assertEq(msToNs("-5"), null, "msToNs: negative -> null (rejected)");
// nsToMs: inverse for seeding the input from a URL ns param.
assertEq(nsToMs("10000000"), "10", "nsToMs: 10,000,000ns -> 10 (trailing zeros trimmed)");
assertEq(nsToMs("1500000"), "1.5", "nsToMs: 1,500,000ns -> 1.5");
assertEq(nsToMs(""), "", "nsToMs: empty -> empty");
assertEq(nsToMs(null), "", "nsToMs: null -> empty");
// Round-trip: a value entered in ms survives ms→ns→ms.
assertEq(nsToMs(msToNs("2.5")), "2.5", "ms→ns→ms round-trips");

// ── Summary ──
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
