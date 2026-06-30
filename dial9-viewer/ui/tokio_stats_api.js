"use strict";

// Pure helpers for the Tokio-stats aggregate page (tokio_stats.html):
//  - building the viewer deep link for a poll exemplar, and
//  - formatting the refinement coverage badge.
//
// Factored out (and CommonJS-exported) so they can be unit-tested under Node
// without a browser DOM (see test_tokio_stats_api.js). In the browser they
// attach as globals via the top-level `function` declarations. The refinement
// loop also reuses `nextMaxFiles` from flamegraph_api.js (a shared global).

// Build the viewer deep link for a poll exemplar.
//
// The viewer fetches each `trace=` component (here a single `/api/object`
// request that streams the still-gzipped segment) and gunzips it client-side,
// attaching the bring-your-own-credentials headers from sessionStorage.
//
// NOTE: we deliberately do NOT pass the exemplar's time range (`start`/`end`).
// In the viewer those params are a *hard parse filter*: it re-parses the trace
// keeping only events inside [start, end] (see getParseOptions in viewer.html).
// A poll exemplar is a single sub-millisecond-to-millisecond window inside a
// ~60s segment, so filtering to it drops essentially every event and the page
// loads empty/broken. Until the viewer can open at a time range *without*
// discarding the surrounding events, we link to the whole segment and let the
// user navigate to the poll. (Tracked in the codebase notes / memory.)
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
    canRefineMore,
  };
}
