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
// are NOT selectable prefixes — the prefix is empty. We only treat the
// listing as a date layer when *every* child looks like a date, so a
// bucket that mixes a real prefix with stray date keys keeps showing
// suggestions rather than silently emptying the prefix.
function isDateLayer(prefixes) {
  if (!prefixes || prefixes.length === 0) return false;
  return prefixes.every((p) => /^\d{4}-\d{2}-\d{2}$/.test(lastSegment(p)));
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = { lastSegment, isDateLayer };
}
