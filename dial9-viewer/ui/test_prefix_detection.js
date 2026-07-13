#!/usr/bin/env node
// Verify isDateLayer() recognizes when a bucket's root children are date
// partitions (YYYY-MM-DD/) rather than genuine key prefixes.
//
// Regression test for issue #471: buckets with no key prefix expose date
// partitions directly at the listing root. Those dates must NOT be treated as
// selectable prefixes — the prefix is empty and the trace data starts at the
// date layer.

"use strict";
const { isDateLayer } = require("./prefix_detect.js");

function assert(cond, label) {
    if (!cond) {
        console.error(`✗ ${label}`);
        process.exit(1);
    }
    console.log(`✓ ${label}`);
}

// Root children that are all dates → this is a date layer (no prefix).
assert(
    isDateLayer(["2026-06-11/", "2026-06-12/"]) === true,
    "all date partitions → date layer",
);

// A single date partition is still a date layer (auto-select must not fire).
assert(
    isDateLayer(["2026-06-12/"]) === true,
    "single date partition → date layer",
);

// Trailing slash optional.
assert(
    isDateLayer(["2026-06-12"]) === true,
    "date without trailing slash → date layer",
);

// Genuine key prefixes (service names) are NOT a date layer.
assert(
    isDateLayer(["traces/", "checkout-api/"]) === false,
    "service-name prefixes → not a date layer",
);

// A single real prefix is not a date layer.
assert(
    isDateLayer(["dial9-traces/"]) === false,
    "single real prefix → not a date layer",
);

// Mixed dates + real prefix, dates NOT a majority → not a clean date layer
// (be conservative, keep offering suggestions rather than silently emptying
// the prefix).
assert(
    isDateLayer(["2026-06-12/", "traces/"]) === false,
    "50/50 dates and prefix → not a date layer",
);

// Regression for the gamma bucket that broke browse: many date partitions
// plus dial9's own auxiliary sibling folder (`diagnostics/`, written by crash
// capture; `flamegraph-data/` from on-demand aggregation behaves the same).
// Dates are a strict majority, so this IS a date layer and the prefix must be
// emptied. Before the majority fix, the lone `diagnostics/` made `.every()`
// return false, the dates were offered as prefix suggestions, and searching
// under a `YYYY-MM-DD/` prefix returned zero objects — an empty browse page.
assert(
    isDateLayer([
        "2026-06-11/",
        "2026-06-12/",
        "2026-06-13/",
        "diagnostics/",
    ]) === true,
    "many dates + one auxiliary sibling → date layer",
);
assert(
    isDateLayer([
        "2026-06-11/",
        "2026-06-12/",
        "2026-06-13/",
        "flamegraph-data/",
    ]) === true,
    "many dates + flamegraph-data sibling → date layer",
);

// Empty input → not a date layer.
assert(isDateLayer([]) === false, "empty list → not a date layer");

// Things that merely start with digits but aren't dates.
assert(
    isDateLayer(["2026/", "2026-06/"]) === false,
    "partial date-like segments → not a date layer",
);

console.log("\nAll isDateLayer tests passed");
