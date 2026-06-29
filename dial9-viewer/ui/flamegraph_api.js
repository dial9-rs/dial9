"use strict";

// Pure helpers for the API-mode flamegraph refinement loop in flamegraph.html.
//
// The `/api/flamegraph` endpoint is demand-driven: each request folds a few
// more source files and returns a `coverage` object alongside the tree. The
// client polls repeatedly, re-rendering as coverage climbs, and stops once
// coverage "freezes" (no more files get folded between polls).
//
// These functions are factored out (and CommonJS-exported) so they can be
// unit-tested under Node without a browser DOM. In the browser they attach as
// globals via the top-level `function` declarations.

// Format the coverage badge shown in the stats area.
//
//   { files_matched: 480, files_folded: 12, samples_folded: 41203 }
//     -> "12 / 480 files (2.5%) · 41,203 samples"
//
//   { files_matched: 480, files_folded: 12, samples_folded: 41203,
//     hosts_matched: 40, hosts_folded: 8 }
//     -> "12 / 480 files (2.5%) · 8 / 40 hosts · 41,203 samples"
//
// percent = files_folded / files_matched * 100. Guards against a zero/missing
// denominator so we never render "NaN%". The host fraction tells the user how
// much of the scope's fleet breadth the current sample spans; it is omitted
// when the backend reports no hosts (older responses) or a single-host scope
// (where "1 / 1 hosts" carries no information).
function formatCoverageBadge(coverage) {
  const matched = Number(coverage.files_matched) || 0;
  const folded = Number(coverage.files_folded) || 0;
  const samples = Number(coverage.samples_folded) || 0;
  const totalBytes = Number(coverage.total_bytes) || 0;
  const hostsMatched = Number(coverage.hosts_matched) || 0;
  const hostsFolded = Number(coverage.hosts_folded) || 0;
  const pct = matched > 0 ? (folded / matched) * 100 : 0;
  let s = `${folded.toLocaleString()} / ${matched.toLocaleString()} files ` +
    `(${pct.toFixed(1)}%)`;
  if (hostsMatched > 1) {
    s += ` · ${hostsFolded.toLocaleString()} / ${hostsMatched.toLocaleString()} hosts`;
  }
  s += ` · ${samples.toLocaleString()} samples`;
  if (totalBytes > 0) s += ` · ${formatBytes(totalBytes)}`;
  return s;
}

function formatBytes(bytes) {
  if (bytes < 1024) return bytes + " B";
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + " KB";
  if (bytes < 1024 * 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + " MB";
  return (bytes / (1024 * 1024 * 1024)).toFixed(1) + " GB";
}

// Coverage is "frozen" when files_folded does not increase between two
// consecutive polls. `prev` is the previous coverage object (or null/undefined
// on the first poll, which is never frozen).
function isCoverageFrozen(prev, curr) {
  if (prev == null) return false;
  const prevFolded = Number(prev.files_folded) || 0;
  const currFolded = Number(curr.files_folded) || 0;
  return currFolded <= prevFolded;
}

// Coverage percent (files_folded / files_matched * 100). 0 when the
// denominator is missing/zero, so callers never see NaN.
function coveragePercent(coverage) {
  if (coverage == null) return 0;
  const matched = Number(coverage.files_matched) || 0;
  const folded = Number(coverage.files_folded) || 0;
  return matched > 0 ? (folded / matched) * 100 : 0;
}

// Decide whether progressive refinement should auto-stop. Refinement plateaus
// long before 100% coverage (the sampling cap), and once each poll only nudges
// coverage by a hair it is not worth the continued network traffic. We stop
// after `patience` consecutive polls whose coverage gain is below `minDeltaPct`
// percentage points.
//
// `deltas` is the recent history of per-poll coverage *gains* (newest last), in
// percentage points. Returns true once the last `patience` entries are all
// below `minDeltaPct`. Pure and history-based so it is unit-testable; the caller
// keeps the rolling array.
function shouldAutoStopRefining(deltas, opts) {
  const o = opts || {};
  const minDeltaPct = o.minDeltaPct != null ? o.minDeltaPct : 0.5;
  const patience = o.patience != null ? o.patience : 3;
  if (deltas.length < patience) return false;
  return deltas.slice(-patience).every((d) => Math.abs(d) < minDeltaPct);
}

// Convert an epoch-nanoseconds value to the string a `datetime-local` input
// expects ("YYYY-MM-DDTHH:MM:SS"), interpreting the instant as UTC. The picker
// has no timezone, so we deliberately show UTC wall-clock — S3 trace keys are
// bucketed in UTC, so the user is always reasoning in UTC.
function nsToPickerUtc(ns) {
  if (ns == null || ns === "") return "";
  return new Date(Number(ns) / 1e6).toISOString().slice(0, 19);
}

// Inverse of `nsToPickerUtc`: parse a `datetime-local` value back to epoch
// nanoseconds (as a string), interpreting it as UTC. The `+ "Z"` is the whole
// point: a bare datetime-local string is parsed by `new Date(...)` as *local*
// time, which shifts the query by the viewer's UTC offset and makes the backend
// list prefixes in the wrong hour (the future, in a negative-offset zone like
// US-Eastern). Appending `Z` keeps this symmetric with `nsToPickerUtc` and
// timezone-independent. Returns null for empty input.
function pickerUtcToNs(val) {
  if (!val) return null;
  return Math.floor(new Date(val + "Z").getTime() * 1e6).toString();
}

// Compute the next `max_files` ceiling when the user clicks "Fetch more".
// Each click requests roughly 4x the current depth, rounded up, capped at a
// sane ceiling so a single click can't ask the backend for everything. Always
// asks for at least `min` more than the current fold count so the click makes
// progress even when files_folded is small (or zero).
function nextMaxFiles(currentFolded, opts) {
  const o = opts || {};
  const cap = o.cap != null ? o.cap : 100000;
  const min = o.min != null ? o.min : 16;
  const folded = Number(currentFolded) || 0;
  const target = Math.ceil(folded * 4);
  return Math.min(cap, Math.max(min, target));
}

// --- Data-driven toolbar facet options ---------------------------------------
//
// The `/api/flamegraph` response carries a `metadata` block describing which
// facets actually have data in the scope (`sources_present`,
// `thread_classes_present`, `host_names`). These helpers turn those backend
// facts into `<option>` descriptors `{ value, label }` so the toolbar offers
// only real dimensions instead of a hard-coded option list. Pure + exported so
// they're unit-testable without a DOM.

// Source selector options. `present` is the backend's `sources_present`
// (e.g. ["cpu", "sched"]). We always keep the canonical order cpu → sched, and
// only add an explicit "All" choice when more than one source exists (with one
// source, "All" and that source are identical). Falls back to just "cpu" when
// the backend reports nothing (older responses / empty scope).
function sourceFacetOptions(present) {
  const have = Array.isArray(present) ? present : [];
  const opts = [];
  if (have.includes("cpu")) opts.push({ value: "cpu", label: "CPU" });
  if (have.includes("sched")) opts.push({ value: "sched", label: "Sched" });
  if (opts.length === 0) opts.push({ value: "cpu", label: "CPU" });
  if (opts.length > 1) opts.push({ value: "all", label: "All" });
  return opts;
}

// Thread-class selector options. `present` is the backend's
// `thread_classes_present` (e.g. ["off-worker", "worker"]). An explicit "All"
// (value "") leads, then only the classes that have data. When the backend
// reports nothing we still offer the full set so the control is never empty.
function threadFacetOptions(present) {
  const have = Array.isArray(present) ? present : [];
  const opts = [{ value: "", label: "All" }];
  const known = have.length ? have : ["worker", "off-worker"];
  if (known.includes("worker")) opts.push({ value: "worker", label: "Worker" });
  if (known.includes("off-worker")) {
    opts.push({ value: "off-worker", label: "Off-worker" });
  }
  return opts;
}

// Host selector options. `hostNames` is the backend's `host_names` (the hosts
// present in the scope). The leading "All" (value "") re-applies the original
// scope host set; each named option narrows to that single host. The "All"
// label carries the count so the user knows how many hosts the scope spans.
function hostFacetOptions(hostNames) {
  const names = Array.isArray(hostNames) ? hostNames.slice() : [];
  const allLabel = names.length > 1 ? `All (${names.length} hosts)` : "All";
  const opts = [{ value: "", label: allLabel }];
  for (const h of names) opts.push({ value: h, label: h });
  return opts;
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
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
  };
}
