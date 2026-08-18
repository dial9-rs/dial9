"use strict";

// prefix_detect.js — S3 prefix-discovery heuristics for the trace browser.
//
// Bundled into the trace browser through the typed lib/trace seam and loaded
// via require by unit tests. Keep this dependency-free for both contexts.

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

// The conventional default key prefix dial9 writes traces under. When a bucket
// exposes several key prefixes we can't otherwise disambiguate, prefer this one
// if it's present rather than leaving the user to pick.
const DEFAULT_TRACE_PREFIX = "dial9-traces";

// Pick a sensible prefix to pre-select from a discovered listing, or undefined
// when the user should choose. Returns `dial9-traces` if the listing offers it
// (the conventional default), otherwise the sole prefix when there is exactly
// one. With multiple non-default prefixes we return undefined so the picker
// stays neutral. Prefixes may carry a trailing slash; the returned value never
// does.
function preferredPrefix(prefixes) {
  if (!prefixes || prefixes.length === 0) return undefined;
  const labels = prefixes.map((p) => String(p).replace(/\/+$/, ""));
  if (labels.includes(DEFAULT_TRACE_PREFIX)) return DEFAULT_TRACE_PREFIX;
  if (labels.length === 1) return labels[0];
  return undefined;
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = { lastSegment, isDateLayer, preferredPrefix };
}
