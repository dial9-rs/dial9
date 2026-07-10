"use strict";

// prefix_detect.js — S3 prefix-discovery heuristics for the trace browser.
//
// Shared between index.html (loaded via <script src>) and the unit tests
// (loaded via require). Keep this dependency-free so both contexts can use it.

// Return the last non-empty path segment of an S3 prefix.
// e.g. "traces/2026-06-12/" → "2026-06-12", "traces/" → "traces".
function lastSegment(prefix) {
  return String(prefix)
    .replace(/\/+$/, "")
    .split("/")
    .pop();
}

// Issue #471: detect when a bucket's root children are date partitions
// (YYYY-MM-DD/) rather than genuine key prefixes. The default S3 key
// layout is `{prefix}/{YYYY-MM-DD}/{HHMM}/{service}/…`; when there is no
// prefix, the date layer sits directly at the listing root. Those dates
// are NOT selectable prefixes — the prefix is empty.
//
// We treat the listing as a date layer when date partitions are a strict
// majority of the root children. Requiring *every* child to be a date was
// too strict: dial9 writes auxiliary sibling folders next to the date layer
// (`diagnostics/` from crash capture, `flamegraph-data/` from on-demand
// aggregation), and a handful of those must not stop us recognizing a bucket
// whose trace data lives directly under the dates. We still refuse to empty
// the prefix when dates are only a minority — a real key-prefix bucket with a
// few stray date keys — so an ambiguous 50/50 listing keeps showing
// suggestions rather than silently emptying the prefix.
function isDateLayer(prefixes) {
  if (!prefixes || prefixes.length === 0) return false;
  const dateCount = prefixes.filter((p) =>
    /^\d{4}-\d{2}-\d{2}$/.test(lastSegment(p)),
  ).length;
  return dateCount * 2 > prefixes.length;
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = { lastSegment, isDateLayer };
}
