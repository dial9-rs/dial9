"use strict";

// Pure helpers shared by the Tokio-stats page.

// Build the viewer deep link for a poll exemplar / long poll.
//
// The viewer fetches each `trace=` component (here a single `/api/object`
// request that streams the still-gzipped segment) and gunzips it client-side,
// attaching the bring-your-own-credentials headers from sessionStorage.
//
// To land on the exact poll, we pass a `focus_*` window (start/end + optional
// worker/task). These are DISTINCT from `start`/`end`: the viewer treats
// start/end as a *hard parse filter* that re-parses keeping only events inside
// the window, so pointing them at one poll's
// sub-millisecond window drops every surrounding event and loads an empty page.
// `focus_*` instead pans/zooms the already-parsed trace to the window and
// highlights the task — non-destructive, so the
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

// Map a worker's busyness percentage to a severity color. Busyness = time spent
// in poll() / wall-clock time. ≥80% is red (near-saturated), ≥50% amber
// (moderately loaded), else green. Thresholds are intentionally higher than
// latencyHeat — a worker spending 50% of time polling is busy but not
// necessarily problematic (unlike a single 3ms poll which directly starves).
function busynessHeat(busyPct) {
  if (busyPct >= 80) return "#f85149"; // red — near-saturated
  if (busyPct >= 50) return "#d29922"; // amber — moderately loaded
  return "#3fb950"; // green — has headroom
}

// A host's busyness as a POOLED ratio: Σ busy_ns / Σ span_ns (span_ns =
// observed active time; see worker_activity in tokio_stats.rs) — total poll
// time over total observed worker-time, i.e. the runtime's mean utilization.
//
// NOT the mean of per-worker busy_pct: a mean-of-ratios over-weights workers
// with a tiny observed window (a few polls clustered in time read as spuriously
// "busy"), so a host sampled more sparsely looks busier than one doing more
// work. Pooling weights each worker by its observed time, so those can't
// dominate.
//
// Bounded to ≤100% (per-worker busy_ns ≤ span_ns, so Σ busy_ns ≤ Σ span_ns).
// Returns 0 for a host with no workers or no observed time.
function hostBusyPct(workers) {
  const ws = workers || [];
  let busy = 0, span = 0;
  for (const w of ws) {
    busy += Number(w.busy_ns) || 0;
    span += Number(w.span_ns) || 0;
  }
  return span > 0 ? (busy / span) * 100 : 0;
}

// A host's worker count as { active, total }, rendered "active / total".
//
//   - active = workers actually observed polling (the rows we have).
//   - total  = the runtime's configured worker count. Tokio numbers workers
//     contiguously 0..N-1, so max observed worker_id + 1 recovers it. When some
//     workers never polled in the window, active < total (e.g. "23 / 64"),
//     surfacing idle-but-available capacity that `active` alone would hide.
//
// total ≥ active always (falls back to active when no worker_id is present).
function hostWorkerCounts(workers) {
  const ws = workers || [];
  const active = ws.length;
  let maxId = -1;
  for (const w of ws) {
    const id = Number(w.worker_id);
    if (Number.isFinite(id) && id > maxId) maxId = id;
  }
  const total = maxId >= 0 ? Math.max(maxId + 1, active) : active;
  return { active, total };
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    exemplarViewerUrl,
    latencyHeat,
    busynessHeat,
    hostBusyPct,
    hostWorkerCounts,
  };
}
