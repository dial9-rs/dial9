#!/usr/bin/env node
"use strict";

// Unit tests for the Tokio-stats aggregate-page helpers in tokio_stats_api.js:
// the exemplar viewer deep link and the refinement coverage badge. These are
// the pure pieces of tokio_stats.html, factored out so they can be tested
// without a browser DOM.

const {
  exemplarViewerUrl,
  formatTokioCoverage,
  latencyHeat,
  busynessHeat,
  hostBusyPct,
  hostWorkerCounts,
  canRefineMore,
} = require("./tokio_stats_api.js");

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

// Float-tolerant variant for computed percentages.
function assertClose(actual, expected, desc, eps = 1e-9) {
  if (Math.abs(actual - expected) <= eps) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(
      `✗ ${desc}\n    expected: ~${JSON.stringify(expected)}\n    actual:   ${JSON.stringify(actual)}`,
    );
    failed++;
  }
}

// ── exemplarViewerUrl ──
// The exemplar link MUST point at /api/object (the route that exists), one key
// per `trace=` component — not the old /api/trace?keys=… shape, which 404s.
//
// It MUST NOT forward the exemplar's time window: the viewer treats start/end
// as a hard parse filter, and a single poll's window is so narrow it drops
// every surrounding event and loads an empty/broken page. So we link to the
// whole segment and omit start/end (and the legacy start_ns/end_ns) entirely.
{
  const url = exemplarViewerUrl({
    bucket: "my-bucket",
    sourceKey: "2026-06-28/2300/shale/host-a/boot-1/123-1306.bin.gz",
    startNs: "1782687631093903000",
    endNs: "1782687631102430000",
  });
  const q = new URLSearchParams(url.slice(url.indexOf("?") + 1));
  assertEq(url.startsWith("viewer.html?"), true, "targets the viewer page");
  const trace = q.get("trace");
  assertEq(
    trace,
    "/api/object?bucket=my-bucket&key=2026-06-28%2F2300%2Fshale%2Fhost-a%2Fboot-1%2F123-1306.bin.gz",
    "trace component is a single /api/object request with the encoded key",
  );
  // The route the bug report hit must NOT appear.
  assertEq(/\/api\/trace\b/.test(url), false, "does not use the nonexistent /api/trace route");
  assertEq(q.has("keys"), false, "does not use the plural keys= param");
  // The time window must NOT be forwarded — it would filter the parse to an
  // empty window and break the page.
  assertEq(q.has("start"), false, "does not forward start (would empty-filter the parse)");
  assertEq(q.has("end"), false, "does not forward end (would empty-filter the parse)");
  assertEq(q.has("start_ns"), false, "does not use start_ns either");
  assertEq(q.has("end_ns"), false, "does not use end_ns either");
}

// Optional svc/host metadata flow through for the viewer title.
{
  const url = exemplarViewerUrl({
    bucket: "b",
    sourceKey: "k.bin.gz",
    svc: "shale",
    host: "host-a",
    startNs: 100,
    endNs: 200,
  });
  const q = new URLSearchParams(url.slice(url.indexOf("?") + 1));
  assertEq(q.get("svc"), "shale", "svc carried through");
  assertEq(q.get("host"), "host-a", "host carried through");
  // Even when a caller passes a window, it is not emitted.
  assertEq(q.has("start"), false, "window dropped even when provided");
}

// A focus window lands on the exact poll via the NON-DESTRUCTIVE focus_* params
// (never start/end): focus_start pans the view, worker/task/end refine it.
{
  const url = exemplarViewerUrl({
    bucket: "b",
    sourceKey: "k.bin.gz",
    focusStartNs: "1782687631093903000",
    focusEndNs: "1782687631102430000",
    focusWorker: 3,
    focusTask: 42,
  });
  const q = new URLSearchParams(url.slice(url.indexOf("?") + 1));
  assertEq(q.get("focus_start"), "1782687631093903000", "focus_start emitted");
  assertEq(q.get("focus_end"), "1782687631102430000", "focus_end emitted");
  assertEq(q.get("focus_worker"), "3", "focus_worker emitted");
  assertEq(q.get("focus_task"), "42", "focus_task emitted");
  // Must still be the focus params, NOT the destructive parse filter.
  assertEq(q.has("start"), false, "focus link does not emit start");
  assertEq(q.has("end"), false, "focus link does not emit end");
}

// focus_start alone is enough (spawn-loc exemplars have no worker/task); the
// refining params are simply omitted rather than sent empty.
{
  const url = exemplarViewerUrl({ bucket: "b", sourceKey: "k.bin.gz", focusStartNs: 500 });
  const q = new URLSearchParams(url.slice(url.indexOf("?") + 1));
  assertEq(q.get("focus_start"), "500", "focus_start emitted on its own");
  assertEq(q.has("focus_end"), false, "focus_end omitted when absent");
  assertEq(q.has("focus_worker"), false, "focus_worker omitted when absent");
  assertEq(q.has("focus_task"), false, "focus_task omitted when absent");
}

// No focus window (plain segment link) emits no focus_* params at all.
{
  const url = exemplarViewerUrl({ bucket: "b", sourceKey: "k.bin.gz" });
  const q = new URLSearchParams(url.slice(url.indexOf("?") + 1));
  assertEq(q.has("focus_start"), false, "no focus params without a window");
}

assertEq(exemplarViewerUrl({ bucket: "b" }), "", "no source key -> empty link");
assertEq(exemplarViewerUrl({}), "", "no opts -> empty link");

// ── formatTokioCoverage ──
assertEq(
  formatTokioCoverage({ files_matched: 480, files_folded: 24 }, 1234567),
  "24 / 480 files (5.0%) · 1,234,567 polls",
  "files + poll count",
);
assertEq(
  formatTokioCoverage(
    { files_matched: 480, files_folded: 24, hosts_matched: 40, hosts_folded: 8 },
    1234567,
  ),
  "24 / 480 files (5.0%) · 8 / 40 hosts · 1,234,567 polls",
  "host spread shown for multi-host scope",
);
assertEq(
  formatTokioCoverage(
    { files_matched: 5, files_folded: 2, hosts_matched: 1, hosts_folded: 1 },
    99,
  ),
  "2 / 5 files (40.0%) · 99 polls",
  "single-host scope omits the uninformative host fraction",
);
assertEq(
  formatTokioCoverage({ files_matched: 0, files_folded: 0 }, 0),
  "0 / 0 files (0.0%) · 0 polls",
  "zero denominator does not produce NaN%",
);
assertEq(
  formatTokioCoverage({ files_matched: 10, files_folded: 4 }, null),
  "4 / 10 files (40.0%)",
  "poll count omitted when not provided",
);
assertEq(formatTokioCoverage(null, 100), "", "no coverage -> empty badge");

// ── latencyHeat ──
// Thresholds match IRIS's latencyHeat: ≥3ms red, ≥1ms amber, else green. Input
// is nanoseconds (the wire unit for poll durations).
assertEq(latencyHeat(500_000), "#3fb950", "sub-1ms poll is green");
assertEq(latencyHeat(999_999), "#3fb950", "just under 1ms is still green");
assertEq(latencyHeat(1_000_000), "#d29922", "1ms poll is amber");
assertEq(latencyHeat(2_500_000), "#d29922", "1–3ms poll is amber");
assertEq(latencyHeat(3_000_000), "#f85149", "3ms poll is red");
assertEq(latencyHeat(50_000_000), "#f85149", "50ms poll is red");

// ── busynessHeat ──
// Busyness = poll time / wall-clock. ≥80% red (saturated), ≥50% amber, else green.
assertEq(busynessHeat(30), "#3fb950", "30% busy is green (has headroom)");
assertEq(busynessHeat(49.9), "#3fb950", "just under 50% is still green");
assertEq(busynessHeat(50), "#d29922", "50% busy is amber (moderately loaded)");
assertEq(busynessHeat(75), "#d29922", "75% busy is amber");
assertEq(busynessHeat(80), "#f85149", "80% busy is red (near-saturated)");
assertEq(busynessHeat(99), "#f85149", "99% busy is red");

// ── hostBusyPct ──
// A host's busyness is the POOLED ratio Σ busy_ns / Σ span_ns — total poll time
// over total observed worker-time — NOT a mean of per-worker ratios (which
// over-weights sparse-window workers and reads backwards).
assertClose(
  hostBusyPct([{ busy_ns: 40, span_ns: 100 }, { busy_ns: 60, span_ns: 100 }]),
  50,
  "two equal-span workers -> pooled ratio equals their mean",
);
assertClose(
  hostBusyPct([{ busy_ns: 7325, span_ns: 10000 }]),
  73.25,
  "single worker -> busy_ns / span_ns",
);
// Pooling weights by observed time, so it is NOT the mean of per-worker
// percentages: a 90%-busy worker observed briefly plus a 10%-busy worker
// observed 9× longer pools to 18%, not the naive 50% average. This is the
// bias that made the old mean read backwards.
assertClose(
  hostBusyPct([{ busy_ns: 90, span_ns: 100 }, { busy_ns: 90, span_ns: 900 }]),
  (90 + 90) / (100 + 900) * 100,
  "pooled ratio weights by observed time (18%), not the 50% naive mean",
);
// rcoh's case: 64 workers each 1/64 saturated -> host reads ~1.56%, not 100%.
{
  const workers = Array.from({ length: 64 }, () => ({ busy_ns: 1, span_ns: 64 }));
  assertClose(hostBusyPct(workers), 100 / 64, "64 workers each 1/64 busy -> ~1.56%, not 100%");
}
// Fully-saturated workers pool to 100% and never exceed it (Σbusy ≤ Σspan).
{
  const workers = Array.from({ length: 8 }, () => ({ busy_ns: 100, span_ns: 100 }));
  assertClose(hostBusyPct(workers), 100, "all workers 100% busy -> host 100% (never over)");
}
// Degenerate / defensive inputs.
assertEq(hostBusyPct([]), 0, "no workers -> 0 (no NaN)");
assertEq(hostBusyPct(undefined), 0, "undefined workers -> 0");
assertEq(
  hostBusyPct([{ busy_ns: 5, span_ns: 0 }]),
  0,
  "zero observed time -> 0 (no divide-by-zero)",
);

// ── hostWorkerCounts ──
// active = observed workers; total = configured (max worker_id + 1, since Tokio
// numbers workers 0..N-1). total ≥ active always.
{
  const c = hostWorkerCounts([{ worker_id: 0 }, { worker_id: 1 }, { worker_id: 2 }]);
  assertEq(c.active, 3, "contiguous 0..2 -> active 3");
  assertEq(c.total, 3, "contiguous 0..2 -> total 3 (fully observed)");
}
// rcoh's case: only 23 workers polled, but ids run up to 63 -> "23 / 64".
{
  const ids = [0, 1, 2, 5, 10, 63]; // 6 observed, highest id 63
  const c = hostWorkerCounts(ids.map((id) => ({ worker_id: id })));
  assertEq(c.active, 6, "sparse ids -> active is observed count");
  assertEq(c.total, 64, "sparse ids -> total is max id + 1 (undercount closed)");
}
// Missing worker_id (older part-files): fall back to active, never below it.
{
  const c = hostWorkerCounts([{}, {}]);
  assertEq(c.active, 2, "no worker_id -> active from row count");
  assertEq(c.total, 2, "no worker_id -> total falls back to active");
}
// A single worker 0 -> 1 / 1.
{
  const c = hostWorkerCounts([{ worker_id: 0 }]);
  assertEq(c.total, 1, "worker 0 alone -> total 1");
}
// No workers.
{
  const c = hostWorkerCounts([]);
  assertEq(c.active, 0, "no workers -> active 0");
  assertEq(c.total, 0, "no workers -> total 0");
}

// ── canRefineMore ──
assertEq(canRefineMore({ files_matched: 480, files_folded: 24 }), true, "folded < matched -> more");
assertEq(canRefineMore({ files_matched: 24, files_folded: 24 }), false, "fully folded -> no more");
assertEq(canRefineMore({ files_matched: 24, files_folded: 30 }), false, "over-folded -> no more");
assertEq(canRefineMore(null), false, "no coverage -> no more");

// ── Summary ──
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
