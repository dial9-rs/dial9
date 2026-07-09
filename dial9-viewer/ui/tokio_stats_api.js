"use strict";

// Pure helpers for the Tokio-stats aggregate page (tokio_stats.html):
//  - building the viewer deep link for a poll exemplar, and
//  - formatting the refinement coverage badge.
//
// Factored out (and CommonJS-exported) so they can be unit-tested under Node
// without a browser DOM (see test_tokio_stats_api.js). In the browser they
// attach as globals via the top-level `function` declarations. The refinement
// loop also reuses `nextMaxFiles` from flamegraph_api.js (a shared global).

// Build the viewer deep link for a poll exemplar / long poll.
//
// The viewer fetches each `trace=` component (here a single `/api/object`
// request that streams the still-gzipped segment) and gunzips it client-side,
// attaching the bring-your-own-credentials headers from sessionStorage.
//
// To land on the exact poll, we pass a `focus_*` window (start/end + optional
// worker/task). These are DISTINCT from `start`/`end`: the viewer treats
// start/end as a *hard parse filter* that re-parses keeping only events inside
// the window (getParseOptions in viewer.html), so pointing them at one poll's
// sub-millisecond window drops every surrounding event and loads an empty page.
// `focus_*` instead pans/zooms the already-parsed trace to the window and
// highlights the task (focusOnWindow in viewer.html) — non-destructive, so the
// surrounding context is still there. When no window is given we just open the
// whole segment. NEVER emit start/end here.
//
// Returns "" when there is no source key to link to.
function exemplarViewerUrl(opts) {
  const o = opts || {};
  if (!o.sourceKey) return "";
  // Mirror the landing page's objectTraceUrls(): one `/api/object?bucket=&key=`
  // component, built via URLSearchParams so the key is correctly encoded.
  const oq = new URLSearchParams();
  oq.set("bucket", o.bucket || "");
  oq.set("key", o.sourceKey);
  const traceUrl = "/api/object?" + oq.toString();

  const p = new URLSearchParams();
  p.set("trace", traceUrl);
  if (o.svc) p.set("svc", o.svc);
  if (o.host) p.set("host", o.host);
  // Non-destructive focus on the exact poll. `focus_start` alone is enough to
  // pan the view; worker/task/end refine the framing and highlight.
  if (o.focusStartNs != null) {
    p.set("focus_start", String(o.focusStartNs));
    if (o.focusEndNs != null) p.set("focus_end", String(o.focusEndNs));
    if (o.focusWorker != null) p.set("focus_worker", String(o.focusWorker));
    if (o.focusTask != null) p.set("focus_task", String(o.focusTask));
  }
  return "viewer.html?" + p.toString();
}

// Format the refinement coverage badge shown in the status bar.
//
//   { files_matched: 480, files_folded: 24 }, 1234567
//     -> "24 / 480 files (5.0%) · 1,234,567 polls"
//
//   { files_matched: 480, files_folded: 24, hosts_matched: 40, hosts_folded: 8 }
//     -> "24 / 480 files (5.0%) · 8 / 40 hosts · 1,234,567 polls"
//
// Unlike the flamegraph's formatCoverageBadge, the trailing count is the
// scope's total poll count (`total_polls`), since polls — not samples — are the
// unit this page aggregates. The host fraction is omitted for single-host
// scopes (where "1 / 1 hosts" carries no information).
function formatTokioCoverage(coverage, totalPolls) {
  if (!coverage) return "";
  const matched = Number(coverage.files_matched) || 0;
  const folded = Number(coverage.files_folded) || 0;
  const hostsMatched = Number(coverage.hosts_matched) || 0;
  const hostsFolded = Number(coverage.hosts_folded) || 0;
  const pct = matched > 0 ? (folded / matched) * 100 : 0;
  let s = `${folded.toLocaleString()} / ${matched.toLocaleString()} files ` +
    `(${pct.toFixed(1)}%)`;
  if (hostsMatched > 1) {
    s += ` · ${hostsFolded.toLocaleString()} / ${hostsMatched.toLocaleString()} hosts`;
  }
  if (totalPolls != null) {
    s += ` · ${(Number(totalPolls) || 0).toLocaleString()} polls`;
  }
  return s;
}

// Map a poll/latency duration (ns) to a severity color, matching IRIS's
// `latencyHeat`: ≥3ms red, ≥1ms amber, else green. Hex values are dial9's own
// palette (the .off-cpu / .card.warn / .card.good colors in tokio_stats.html),
// so the heat reads consistently with the rest of the page.
function latencyHeat(ns) {
  const ms = Number(ns) / 1e6;
  if (ms >= 3) return "#f85149"; // red — over the ~3ms hop budget
  if (ms >= 1) return "#d29922"; // amber — 1–3ms
  return "#3fb950"; // green — sub-millisecond
}

// Whether a coverage block still has matched files left to fold (so "Load more"
// can deepen the sample). False when fully folded or coverage is absent.
function canRefineMore(coverage) {
  if (!coverage) return false;
  const matched = Number(coverage.files_matched) || 0;
  const folded = Number(coverage.files_folded) || 0;
  return folded < matched;
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    exemplarViewerUrl,
    formatTokioCoverage,
    latencyHeat,
    canRefineMore,
  };
}
