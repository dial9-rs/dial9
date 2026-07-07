#!/usr/bin/env node
"use strict";

// Unit tests for the Tokio-stats aggregate-page helpers in tokio_stats_api.js:
// the exemplar viewer deep link and the refinement coverage badge. These are
// the pure pieces of tokio_stats.html, factored out so they can be tested
// without a browser DOM.

const {
  exemplarViewerUrl,
  formatTokioCoverage,
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

// ── canRefineMore ──
assertEq(canRefineMore({ files_matched: 480, files_folded: 24 }), true, "folded < matched -> more");
assertEq(canRefineMore({ files_matched: 24, files_folded: 24 }), false, "fully folded -> no more");
assertEq(canRefineMore({ files_matched: 24, files_folded: 30 }), false, "over-folded -> no more");
assertEq(canRefineMore(null), false, "no coverage -> no more");

// ── Summary ──
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
