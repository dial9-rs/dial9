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
// percent = files_folded / files_matched * 100. Guards against a zero/missing
// denominator so we never render "NaN%".
function formatCoverageBadge(coverage) {
  const matched = Number(coverage.files_matched) || 0;
  const folded = Number(coverage.files_folded) || 0;
  const samples = Number(coverage.samples_folded) || 0;
  const pct = matched > 0 ? (folded / matched) * 100 : 0;
  return (
    `${folded.toLocaleString()} / ${matched.toLocaleString()} files ` +
    `(${pct.toFixed(1)}%) · ${samples.toLocaleString()} samples`
  );
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

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    formatCoverageBadge,
    isCoverageFrozen,
    coveragePercent,
    shouldAutoStopRefining,
    nextMaxFiles,
    nsToPickerUtc,
    pickerUtcToNs,
  };
}
