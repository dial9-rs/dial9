#!/usr/bin/env node
"use strict";

// Unit tests for the Span Explorer helpers in span_explorer.js: duration
// formatting, catalog sorting, histogram normalization/layout/brush, time
// composition, URL deep-linking, and flamegraph URL generation. All DOM-free.

const SE = require("./span_explorer.js");

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

function assert(cond, desc) {
  if (cond) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(`✗ ${desc}`);
    failed++;
  }
}


// ── refinement URL and stream-mode invariants ──
{
  const params = new URLSearchParams("api=1");
  SE.setMaxFilesParam(params, 80);
  assertEq(params.get("max_files"), "80", "max_files: requested refinement depth is persisted");
  SE.setMaxFilesParam(params, null);
  assertEq(params.has("max_files"), false, "max_files: reset removes refinement depth");
}
assertEq(
  SE.shouldAdoptCatalogSnapshot("replace", 80, 0),
  true,
  "catalog adoption: replace mode adopts its first snapshot",
);
assertEq(
  SE.shouldAdoptCatalogSnapshot("exemplars", 80, 100),
  false,
  "catalog adoption: exemplar refresh never replaces the catalog",
);
assertEq(
  SE.shouldAdoptCatalogSnapshot("refine", 80, 79),
  false,
  "catalog adoption: refine preserves the catalog below baseline",
);
assertEq(
  SE.shouldAdoptCatalogSnapshot("refine", 80, 80),
  true,
  "catalog adoption: refine adopts at baseline",
);
assertEq(
  SE.shouldAdoptCatalogSnapshot("refine", 80, 96),
  true,
  "catalog adoption: refine adopts progressive snapshots above baseline",
);
// ── fmtNs ──
assertEq(SE.fmtNs(500), "500ns", "fmtNs: sub-µs");
assertEq(SE.fmtNs(1500000), "1.5ms", "fmtNs: 1.5ms");
assertEq(SE.fmtNs(2000000000), "2s", "fmtNs: 2s");
assertEq(SE.fmtNs(null), "—", "fmtNs: null → dash");
assertEq(SE.fmtNs(-1), "—", "fmtNs: negative → dash");
assertEq(SE.fmtNs(0), "0", "fmtNs: zero");

// ── spanTypeLabel ──
assertEq(
  SE.spanTypeLabel({ name: "handle_request", callsite_file: "src/handler.rs", callsite_line: 42 }),
  "handle_request (handler.rs:42)",
  "spanTypeLabel: name + file:line"
);
assertEq(
  SE.spanTypeLabel({ name: "poll", callsite_file: null, callsite_line: null }),
  "poll",
  "spanTypeLabel: name only, no callsite"
);
assertEq(
  SE.spanTypeLabel({}),
  "(unnamed)",
  "spanTypeLabel: empty → unnamed"
);

// ── sortSpanTypes ──
{
  const types = [
    { name: "a", count: 10, p50_ns: 100 },
    { name: "b", count: 50, p50_ns: 200 },
    { name: "c", count: 5, p50_ns: 50 },
  ];
  const byCount = SE.sortSpanTypes(types, "count", false);
  assertEq(byCount[0].name, "b", "sortSpanTypes: desc by count → b first");
  assertEq(byCount[2].name, "c", "sortSpanTypes: desc by count → c last");

  const byCountAsc = SE.sortSpanTypes(types, "count", true);
  assertEq(byCountAsc[0].name, "c", "sortSpanTypes: asc by count → c first");

  const byName = SE.sortSpanTypes(types, "name", true);
  assertEq(byName[0].name, "a", "sortSpanTypes: asc by name → a first");
}

// ── normalizeSpanHistogram ──
{
  const raw = [
    { lo_ns: 1000000, hi_ns: 2000000, count: 5 },
    { lo_ns: 500000, hi_ns: 1000000, count: 10 },
    { lo_ns: 100, hi_ns: 50, count: 1 }, // invalid: hi <= lo
    null,
  ];
  const norm = SE.normalizeSpanHistogram(raw);
  assertEq(norm.length, 2, "normalizeSpanHistogram: drops invalid + null");
  assertEq(norm[0].lo_ns, 500000, "normalizeSpanHistogram: sorts ascending");
  assertEq(norm[1].count, 5, "normalizeSpanHistogram: preserves count");
}

// ── spanHistogramLayout ──
{
  const bars = [
    { lo_ns: 1000000, hi_ns: 2000000, count: 20 },
    { lo_ns: 2000000, hi_ns: 4000000, count: 10 },
  ];
  const { cols, maxCount } = SE.spanHistogramLayout(bars, 200, 2);
  assertEq(cols.length, 2, "spanHistogramLayout: 2 bars → 2 cols");
  assertEq(maxCount, 20, "spanHistogramLayout: maxCount = 20");
  assertEq(cols[0].x, 0, "spanHistogramLayout: first col at x=0");
  assertEq(cols[0].w, 98, "spanHistogramLayout: col width = 200/2 - gap");
  assertEq(cols[0].hFrac, 1, "spanHistogramLayout: tallest col has hFrac=1");
  assertEq(cols[1].hFrac, 0.5, "spanHistogramLayout: second col hFrac=0.5");
}

// ── spanPxToNs ──
{
  const bars = [
    { lo_ns: 1000000, hi_ns: 2000000, count: 10 },
    { lo_ns: 2000000, hi_ns: 4000000, count: 5 },
  ];
  const ns0 = SE.spanPxToNs(bars, 200, 0);
  assertEq(ns0, 1000000, "spanPxToNs: x=0 → lo_ns of first bar");
  const nsEnd = SE.spanPxToNs(bars, 200, 200);
  assertEq(nsEnd, 4000000, "spanPxToNs: x=width → hi_ns of last bar");
  const nsMid = SE.spanPxToNs(bars, 200, 100);
  assert(nsMid >= 2000000 && nsMid <= 2000001, "spanPxToNs: x=mid → boundary between bars");
}

// ── durationAtPercentile / spanNsToPx ──
{
  // 30 instances across three log buckets, 10 each.
  const bars = [
    { lo_ns: 1000000, hi_ns: 2000000, count: 10 },
    { lo_ns: 2000000, hi_ns: 4000000, count: 10 },
    { lo_ns: 4000000, hi_ns: 8000000, count: 10 },
  ];
  // p0 → floor of first bar; p100 → ceiling of last bar.
  assertEq(SE.durationAtPercentile(bars, 0), 1000000, "durationAtPercentile: p0 = min");
  assertEq(SE.durationAtPercentile(bars, 100), 8000000, "durationAtPercentile: p100 = max");
  // p50 lands at the 15th instance → halfway (by count) into the 2nd bar, which
  // on the log axis is its geometric midpoint: 2ms·√2 ≈ 2.83ms.
  const p50 = SE.durationAtPercentile(bars, 50);
  assert(Math.abs(p50 - 2000000 * Math.SQRT2) < 5, "durationAtPercentile: p50 = geometric mid of 2nd bucket");
  // durationAtPercentile is the inverse of percentileForDuration (round-trip).
  const p90 = SE.durationAtPercentile(bars, 90);
  assert(Math.abs(SE.percentileForDuration(bars, p90) - 90) < 1e-4, "percentile/duration round-trip at p90");
  assertEq(SE.durationAtPercentile([], 50), null, "durationAtPercentile: empty → null");

  // spanNsToPx is the inverse of spanPxToNs on the same axis.
  const W = 300;
  const midNs = SE.spanPxToNs(bars, W, 150);
  assert(Math.abs(SE.spanNsToPx(bars, W, midNs) - 150) < 1e-4, "spanNsToPx: inverts spanPxToNs at mid");
  assertEq(SE.spanNsToPx(bars, W, 500000), 0, "spanNsToPx: below min clamps to 0");
  assertEq(SE.spanNsToPx(bars, W, 16000000), W, "spanNsToPx: above max clamps to width");
  assertEq(SE.spanNsToPx([], W, 5), null, "spanNsToPx: empty → null");
}

// ── spanBrushToBand ──
{
  const bars = [
    { lo_ns: 1000000, hi_ns: 2000000, count: 10 },
    { lo_ns: 2000000, hi_ns: 4000000, count: 5 },
  ];
  const band = SE.spanBrushToBand(bars, 200, 10, 150);
  assert(band != null, "spanBrushToBand: non-zero drag → non-null band");
  assert(band.min_ns >= 1000000, "spanBrushToBand: min_ns >= first bar lo");
  assert(band.max_ns <= 4000000, "spanBrushToBand: max_ns <= last bar hi");
  assert(band.min_ns < band.max_ns, "spanBrushToBand: min < max");

  const click = SE.spanBrushToBand(bars, 200, 50, 51);
  assertEq(click, null, "spanBrushToBand: near-zero drag → null (treated as click)");
}

// ── countInBand ──
{
  const bars = [
    { lo_ns: 1000000, hi_ns: 2000000, count: 10 },
    { lo_ns: 2000000, hi_ns: 4000000, count: 5 },
    { lo_ns: 4000000, hi_ns: 8000000, count: 3 },
  ];
  assertEq(SE.countInBand(bars, null, null), 18, "countInBand: no bounds → all");
  assertEq(SE.countInBand(bars, 2000000, 4000000), 5, "countInBand: exact one bar");
  assertEq(SE.countInBand(bars, 1500000, 3000000), 15, "countInBand: overlapping two bars");
  assertEq(SE.countInBand(bars, 10000000, 20000000), 0, "countInBand: no overlap → 0");
}

// ── computeTimeComposition ──
{
  // Fallback: no backend composition → single Unknown category
  const st = {
    histogram: [
      { lo_ns: 1000000, hi_ns: 2000000, count: 10 },
    ],
  };
  const comp = SE.computeTimeComposition(st);
  assert(comp.total_ns > 0, "computeTimeComposition fallback: total_ns > 0");
  assertEq(comp.categories.length, 1, "computeTimeComposition fallback: one Unknown category");
  assertEq(comp.categories[0].key, "unknown", "computeTimeComposition fallback: key is 'unknown'");
  assertEq(comp.categories[0].frac, 1.0, "computeTimeComposition fallback: frac is 1.0");
}

{
  // With backend composition data: five categories
  const st = {
    histogram: [{ lo_ns: 1000000, hi_ns: 2000000, count: 10 }],
    composition: {
      on_cpu_ns: 5000000,
      blocked_ns: 2000000,
      async_wait_ns: 1000000,
      scheduler_delay_ns: 500000,
      unknown_ns: 1500000,
    },
  };
  const comp = SE.computeTimeComposition(st);
  assertEq(comp.total_ns, 10000000, "computeTimeComposition with data: total_ns = sum of fields");
  assertEq(comp.categories.length, 5, "computeTimeComposition with data: five categories");
  assertEq(comp.categories[0].key, "on_cpu", "computeTimeComposition with data: first is on_cpu");
  assertEq(comp.categories[0].ns, 5000000, "computeTimeComposition with data: on_cpu_ns correct");
  assertEq(comp.categories[0].frac, 0.5, "computeTimeComposition with data: on_cpu frac = 0.5");
  assertEq(comp.categories[4].key, "unknown", "computeTimeComposition with data: last is unknown");
  assertEq(comp.categories[4].ns, 1500000, "computeTimeComposition with data: unknown_ns correct");
  assert(Math.abs(comp.categories[4].frac - 0.15) < 1e-9, "computeTimeComposition with data: unknown frac ≈ 0.15");
}

{
  // All zeros in composition → degenerate case
  const st = {
    histogram: [],
    composition: { on_cpu_ns: 0, blocked_ns: 0, async_wait_ns: 0, scheduler_delay_ns: 0, unknown_ns: 0 },
  };
  const comp = SE.computeTimeComposition(st);
  assertEq(comp.total_ns, 0, "computeTimeComposition zeros: total_ns = 0");
  assertEq(comp.categories.length, 1, "computeTimeComposition zeros: single Unknown category");
  assertEq(comp.categories[0].key, "unknown", "computeTimeComposition zeros: key is unknown");
}

{
  // Composition with only on_cpu (other fields zero) → unknown still present at 0
  const st = {
    histogram: [],
    composition: { on_cpu_ns: 1000, blocked_ns: 0, async_wait_ns: 0, scheduler_delay_ns: 0, unknown_ns: 0 },
  };
  const comp = SE.computeTimeComposition(st);
  assertEq(comp.total_ns, 1000, "computeTimeComposition on_cpu only: total = 1000");
  assertEq(comp.categories.length, 5, "computeTimeComposition on_cpu only: always five categories");
  assertEq(comp.categories[0].frac, 1.0, "computeTimeComposition on_cpu only: on_cpu frac = 1.0");
  assertEq(comp.categories[4].frac, 0, "computeTimeComposition on_cpu only: unknown frac = 0");
  assertEq(comp.categories[4].ns, 0, "computeTimeComposition on_cpu only: unknown ns = 0");
}

// ── equal-weighted composition (per-instance fractions) ──
{
  // The motivating case: 10 short spans that are ~100% on-CPU, plus ONE long
  // span that is ~100% async-wait. Time-weighted, the single long span's ns
  // dominates; equal-weighted, on-CPU should be the ~91% majority (10 of 11
  // instances). The backend provides instance_count + per-category frac sums.
  //   - on_cpu_frac_sum ≈ 10 (ten instances each 100% on-CPU)
  //   - async_wait_frac_sum ≈ 1 (one instance 100% wait)
  //   - instance_count = 11
  const st = {
    histogram: [
      { lo_ns: 1000, hi_ns: 2000, count: 10 },
      { lo_ns: 200000000, hi_ns: 400000000, count: 1 },
    ],
    composition: {
      on_cpu_ns: 15000,          // 10 × ~1500ns
      blocked_ns: 0,
      async_wait_ns: 200000000,  // one 200ms wait dominates the ns total
      scheduler_delay_ns: 0,
      unknown_ns: 0,
      instance_count: 11,
      on_cpu_frac_sum: 10,
      blocked_frac_sum: 0,
      async_wait_frac_sum: 1,
      scheduler_delay_frac_sum: 0,
      unknown_frac_sum: 0,
    },
  };
  const comp = SE.computeTimeComposition(st, null);
  assertEq(comp.weighting, "equal", "equal-weighted: reports equal weighting when instance data present");
  const onCpu = comp.categories.find((c) => c.key === "on_cpu");
  const wait = comp.categories.find((c) => c.key === "async_wait");
  // Equal-weighted share: on-CPU is 10/11 ≈ 0.909, NOT the ~0% a time-weighted
  // view would show (15000 / 200015000).
  assert(Math.abs(onCpu.frac - 10 / 11) < 1e-9, "equal-weighted: on-CPU ≈ 10/11 (not dominated by long span)");
  assert(Math.abs(wait.frac - 1 / 11) < 1e-9, "equal-weighted: async-wait ≈ 1/11");
  // Actual total time is still preserved (task 3), and mean per instance.
  assertEq(wait.ns, 200000000, "equal-weighted: preserves actual total wait ns");
  assert(Math.abs(onCpu.meanNs - 15000 / 11) < 1e-6, "equal-weighted: meanNs = total / instance_count");
}

// ── band-scoped composition (composition_histogram) ──
{
  // Two duration buckets with distinct compositions: short spans are all
  // on-CPU, long spans are all async-wait. Brushing to the long bucket must
  // yield an all-wait composition, not the blended whole-type total.
  const st = {
    histogram: [
      { lo_ns: 1000, hi_ns: 2000, count: 3 },
      { lo_ns: 1000000, hi_ns: 2000000, count: 1 },
    ],
    composition: {
      on_cpu_ns: 300,
      blocked_ns: 0,
      async_wait_ns: 1000000,
      scheduler_delay_ns: 0,
      unknown_ns: 0,
    },
    composition_histogram: [
      { lo_ns: 1000, hi_ns: 2000, on_cpu_ns: 300, blocked_ns: 0, async_wait_ns: 0, scheduler_delay_ns: 0, unknown_ns: 0 },
      { lo_ns: 1000000, hi_ns: 2000000, on_cpu_ns: 0, blocked_ns: 0, async_wait_ns: 1000000, scheduler_delay_ns: 0, unknown_ns: 0 },
    ],
  };

  // No band → whole-type total (on_cpu 300 + wait 1e6).
  const all = SE.computeTimeComposition(st, null);
  assertEq(all.total_ns, 1000300, "band comp: no band → whole-type total");

  // Brush the long bucket [500000, 3000000): only the wait bucket overlaps.
  const longBand = SE.computeTimeComposition(st, { min_ns: 500000, max_ns: 3000000 });
  assertEq(longBand.total_ns, 1000000, "band comp: long band total = wait bucket only");
  const wait = longBand.categories.find((c) => c.key === "async_wait");
  assertEq(wait.frac, 1.0, "band comp: long band is 100% async wait");
  const oncpuLong = longBand.categories.find((c) => c.key === "on_cpu");
  assertEq(oncpuLong.ns, 0, "band comp: long band has no on-CPU");

  // Brush the short bucket [0, 5000): only the on-CPU bucket overlaps.
  const shortBand = SE.computeTimeComposition(st, { min_ns: 0, max_ns: 5000 });
  assertEq(shortBand.total_ns, 300, "band comp: short band total = on-CPU bucket only");
  const oncpuShort = shortBand.categories.find((c) => c.key === "on_cpu");
  assertEq(oncpuShort.frac, 1.0, "band comp: short band is 100% on-CPU");

  // bandComposition helper: overlap rule (hi > min && lo < max), open-ended max.
  const openHi = SE.bandComposition(st, 500000, null);
  assertEq(openHi.async_wait_ns, 1000000, "bandComposition: open-ended max sums the long bucket");
  assertEq(openHi.on_cpu_ns, 0, "bandComposition: open-ended max excludes the short bucket");
}

{
  // A band selected but NO composition_histogram (older backend): falls back to
  // the whole-type composition total rather than returning nothing.
  const st = {
    histogram: [{ lo_ns: 1000, hi_ns: 2000, count: 1 }],
    composition: { on_cpu_ns: 100, blocked_ns: 0, async_wait_ns: 0, scheduler_delay_ns: 0, unknown_ns: 0 },
  };
  assertEq(SE.bandComposition(st, 0, 5000), null, "bandComposition: null when no composition_histogram");
  const comp = SE.computeTimeComposition(st, { min_ns: 0, max_ns: 5000 });
  assertEq(comp.total_ns, 100, "computeTimeComposition: band with no per-bucket data → whole-type total");
}

{
  // Composition with null/undefined fields treated as 0
  const st = {
    histogram: [],
    composition: { on_cpu_ns: 1000, blocked_ns: null, async_wait_ns: undefined, scheduler_delay_ns: 0, unknown_ns: 500 },
  };
  const comp = SE.computeTimeComposition(st);
  assertEq(comp.total_ns, 1500, "computeTimeComposition partial nulls: total = on_cpu + unknown");
  assertEq(comp.categories[0].ns, 1000, "computeTimeComposition partial nulls: on_cpu_ns = 1000");
  assertEq(comp.categories[1].ns, 0, "computeTimeComposition partial nulls: blocked treated as 0");
}

// ── spanTypeQuality ──
{
  assertEq(SE.spanTypeQuality({}), null, "spanTypeQuality: no fields → null");
  assertEq(SE.spanTypeQuality({ details_complete_count: 80, partial_count: 20 }), 0.8, "spanTypeQuality: 80/100 = 0.8");
  assertEq(SE.spanTypeQuality({ details_complete_count: 0, partial_count: 0 }), null, "spanTypeQuality: 0+0 → null");
  assertEq(SE.spanTypeQuality({ details_complete_count: 100, partial_count: 0 }), 1.0, "spanTypeQuality: all complete = 1.0");
  assertEq(SE.spanTypeQuality({ details_complete_count: 0, partial_count: 50 }), 0, "spanTypeQuality: none complete = 0");
}

// ── encodeSpanExplorerState / decodeSpanExplorerState ──
{
  const state = {
    data_dir: "/tmp/dial9-traces",
    max_files: 80,
    bucket: "my-bucket",
    region: "us-west-2",
    credentialMode: "role",
    roleArn: "arn:aws:iam::123456789012:role/Dial9TraceReader",
    prefix: "traces/",
    service: "my-svc",
    hosts: ["host-a", "host-b"],
    start_ns: "1700000000000000000",
    end_ns: "1700001000000000000",
    span_type_uid: "abcdef0123456789abcdef0123456789",
    min_span_ns: 1000000,
    max_span_ns: 5000000,
  };
  const encoded = SE.encodeSpanExplorerState(state);
  assertEq(encoded.get("data_dir"), "/tmp/dial9-traces", "encodeState: local data_dir");
  assertEq(encoded.get("max_files"), "80", "encodeState: refinement depth");
  assertEq(encoded.get("bucket"), "my-bucket", "encodeState: bucket");
  assertEq(encoded.get("aws_region"), "us-west-2", "encodeState: region");
  assertEq(encoded.get("credential_mode"), "role", "encodeState: credential mode");
  assertEq(encoded.get("aws_role_arn"), state.roleArn, "encodeState: role ARN");
  assertEq(encoded.get("span_type_uid"), state.span_type_uid, "encodeState: span_type_uid");
  assertEq(encoded.get("min_span_ns"), "1000000", "encodeState: min_span_ns");
  assertEq(encoded.getAll("host").length, 2, "encodeState: hosts repeatable");

  const decoded = SE.decodeSpanExplorerState(encoded);
  assertEq(decoded.data_dir, "/tmp/dial9-traces", "decodeState: local data_dir");
  assertEq(decoded.max_files, 80, "decodeState: refinement depth");
  assertEq(decoded.bucket, "my-bucket", "decodeState: bucket");
  assertEq(decoded.credentialMode, "role", "decodeState: credential mode");
  assertEq(decoded.roleArn, state.roleArn, "decodeState: role ARN");
  assertEq(decoded.span_type_uid, state.span_type_uid, "decodeState: span_type_uid");
  assertEq(decoded.min_span_ns, 1000000, "decodeState: min_span_ns");
  assertEq(decoded.hosts.length, 2, "decodeState: hosts");
}

// ── flamegraphUrl ──
{
  const state = {
    data_dir: "/tmp/dial9-traces",
    max_files: 80,
    bucket: "bkt",
    region: null,
    credentialMode: "ambient",
    prefix: null,
    service: "svc",
    hosts: ["h1"],
    start_ns: "100",
    end_ns: "200",
    span_type_uid: "aabb",
    min_span_ns: 1000,
    max_span_ns: 5000,
  };
  const cpuUrl = SE.flamegraphUrl(state, "on_cpu");
  assert(cpuUrl.includes("data_dir=%2Ftmp%2Fdial9-traces"), "flamegraphUrl: carries local data_dir");
  assert(cpuUrl.includes("max_files=80"), "flamegraphUrl: carries refinement depth");
  assert(cpuUrl.includes("source=cpu"), "flamegraphUrl: on_cpu → source=cpu");
  assert(cpuUrl.includes("credential_mode=ambient"), "flamegraphUrl: carries ambient mode");
  assert(cpuUrl.includes("span_type_uid=aabb"), "flamegraphUrl: carries span_type_uid");
  assert(cpuUrl.includes("min_span_ns=1000"), "flamegraphUrl: carries min_span_ns");
  assert(cpuUrl.startsWith("flamegraph.html?"), "flamegraphUrl: points at flamegraph.html");

  const blockUrl = SE.flamegraphUrl(state, "blocking");
  assert(blockUrl.includes("source=sched"), "flamegraphUrl: blocking → source=sched");
}

// ── exemplarViewerUrl ──
{
  const scope = {
    bucket: "my-bucket",
    region: "us-west-2",
    credentialMode: "role",
    roleArn: "arn:aws:iam::123456789012:role/Dial9TraceReader",
    service: "my-svc",
    spanName: "/jobs/next",
  };
  const exemplar = {
    elapsed_ns: 5000000,
    span_uid: "deadbeef",
    host: "host-7",
    start_ns: 1700000000000000000,
    end_ns: 1700000000005000000,
    source_key: "2026-06-19/1450/shale/host-7/boot-1/123-0.bin.gz",
  };
  const url = SE.exemplarViewerUrl(exemplar, scope);
  assert(url.startsWith("viewer.html?"), "exemplarViewerUrl: points at viewer.html");
  // Decode the query to inspect the params robustly (values are URL-encoded).
  const qs = new URLSearchParams(url.slice("viewer.html?".length));
  const traceParam = qs.get("trace");
  assert(traceParam != null, "exemplarViewerUrl: has trace param");
  assert(traceParam.startsWith("/api/object?"), "exemplarViewerUrl: trace is an /api/object component");
  const traceQs = new URLSearchParams(traceParam.slice("/api/object?".length));
  assertEq(traceQs.get("bucket"), "my-bucket", "exemplarViewerUrl: trace carries bucket");
  assertEq(traceQs.get("key"), exemplar.source_key, "exemplarViewerUrl: trace carries source_key");
  assertEq(qs.get("svc"), "my-svc", "exemplarViewerUrl: carries svc");
  assertEq(qs.get("host"), "host-7", "exemplarViewerUrl: uses the exemplar's own host");
  assertEq(qs.get("aws_region"), "us-west-2", "exemplarViewerUrl: carries region");
  assertEq(qs.get("credential_mode"), "role", "exemplarViewerUrl: carries role mode");
  assertEq(qs.get("aws_role_arn"), scope.roleArn, "exemplarViewerUrl: carries role ARN");
  assertEq(qs.get("focus_start"), String(exemplar.start_ns), "exemplarViewerUrl: focus_start from start_ns");
  assertEq(qs.get("focus_end"), String(exemplar.end_ns), "exemplarViewerUrl: focus_end from end_ns");
  assertEq(qs.get("focus_span_name"), "/jobs/next", "exemplarViewerUrl: forwards focus_span_name from scope");
  // Must NOT emit hard parse-filter start/end (would load an empty viewer).
  assertEq(qs.get("start"), null, "exemplarViewerUrl: never emits hard start filter");
  assertEq(qs.get("end"), null, "exemplarViewerUrl: never emits hard end filter");
}

{
  // focus_span_name is omitted when the scope has no spanName (backwards compat).
  const url = SE.exemplarViewerUrl(
    { elapsed_ns: 1, start_ns: 5, end_ns: 6, host: "h", source_key: "k" },
    { bucket: "b", service: "s" }
  );
  const qs = new URLSearchParams(url.slice("viewer.html?".length));
  assertEq(qs.get("focus_span_name"), null, "exemplarViewerUrl: no focus_span_name without scope.spanName");
}

{
  // Regression: the backend stores source_key as a fully-qualified
  // `s3://{bucket}/{key}` URI (the decode `full_key`). The link must split it
  // into bucket + BUCKET-RELATIVE key — passing the whole `s3://…` as `key=`
  // made /api/object 404. The bucket from the URI wins over the scope bucket.
  const url = SE.exemplarViewerUrl(
    {
      elapsed_ns: 5000000,
      host: "host-9",
      start_ns: 1700000000000000000,
      end_ns: 1700000000005000000,
      source_key:
        "s3://cell2-beta-pdx-dial9-traces/2026-07-15/1715/shale/host-9/myvk-1/1784135701-469.bin.gz",
    },
    { bucket: "scope-bucket", region: "us-west-2", service: "shale" }
  );
  const qs = new URLSearchParams(url.slice("viewer.html?".length));
  const traceQs = new URLSearchParams(qs.get("trace").slice("/api/object?".length));
  assertEq(
    traceQs.get("bucket"),
    "cell2-beta-pdx-dial9-traces",
    "exemplarViewerUrl: bucket parsed from s3:// URI (not scope bucket)"
  );
  assertEq(
    traceQs.get("key"),
    "2026-07-15/1715/shale/host-9/myvk-1/1784135701-469.bin.gz",
    "exemplarViewerUrl: key is bucket-relative (s3://bucket/ stripped)"
  );
  assert(
    !traceQs.get("key").startsWith("s3://"),
    "exemplarViewerUrl: key must not retain the s3:// scheme"
  );
}

{
  // No source_key → empty string (nothing to link to).
  const url = SE.exemplarViewerUrl(
    { elapsed_ns: 1000, start_ns: 1, end_ns: 2, host: "h" },
    { bucket: "b" }
  );
  assertEq(url, "", "exemplarViewerUrl: no source_key → empty string");
}

{
  // Raw mode already has the original trace URL, so it does not need a
  // backend source_key to build a focused viewer link.
  const url = SE.exemplarViewerUrl(
    { elapsed_ns: 1000, start_ns: 10, end_ns: 20 },
    { trace: "demo-trace.bin", region: "us-west-2", spanName: "RecordMetric" }
  );
  const qs = new URLSearchParams(url.slice("viewer.html?".length));
  assertEq(qs.get("trace"), "demo-trace.bin", "exemplarViewerUrl: raw trace is reused");
  assertEq(qs.get("focus_start"), "10", "exemplarViewerUrl: raw trace focus_start");
  assertEq(qs.get("focus_end"), "20", "exemplarViewerUrl: raw trace focus_end");
  assertEq(qs.get("aws_region"), "us-west-2", "exemplarViewerUrl: raw trace region");
  assertEq(
    qs.get("focus_span_name"),
    "RecordMetric",
    "exemplarViewerUrl: raw trace forwards span name"
  );
}

{
  // Missing focus window: start_ns absent → no focus_start (still a valid link).
  const url = SE.exemplarViewerUrl(
    { elapsed_ns: 1000, source_key: "k", host: "h" },
    { bucket: "b", service: "s" }
  );
  const qs = new URLSearchParams(url.slice("viewer.html?".length));
  assertEq(qs.get("focus_start"), null, "exemplarViewerUrl: no start_ns → no focus_start");
  assertEq(qs.get("host"), "h", "exemplarViewerUrl: still carries host without focus");
}


// ── exemplarsInBand ──
{
  const exemplars = [
    { elapsed_ns: 50, span_uid: "slow" },
    { elapsed_ns: 30, span_uid: "middle" },
    { elapsed_ns: 10, span_uid: "fast" },
  ];
  assertEq(
    SE.exemplarsInBand(exemplars, 10, 30).map((e) => e.span_uid).join(","),
    "middle,fast",
    "exemplarsInBand: applies inclusive duration bounds",
  );
  assertEq(
    SE.exemplarsInBand(exemplars, 31, null).map((e) => e.span_uid).join(","),
    "slow",
    "exemplarsInBand: supports an open maximum",
  );
  assertEq(
    SE.exemplarsInBand(exemplars, null, null).length,
    3,
    "exemplarsInBand: no bounds preserves all candidates",
  );
}

// ── percentileForDuration / fmtPercentile ──
{
  // 30 instances: 10 in [1ms,2ms), 10 in [2ms,4ms), 10 in [4ms,8ms).
  const hist = [
    { lo_ns: 1000000, hi_ns: 2000000, count: 10 },
    { lo_ns: 2000000, hi_ns: 4000000, count: 10 },
    { lo_ns: 4000000, hi_ns: 8000000, count: 10 },
  ];
  // A value at/above the top bar's ceiling → 100th percentile.
  assertEq(SE.percentileForDuration(hist, 8000000), 100, "percentileForDuration: at max → 100");
  // A value at the boundary between bar 1 and bar 2 → 1/3 below.
  const pMid = SE.percentileForDuration(hist, 2000000);
  assert(Math.abs(pMid - 100 / 3) < 1e-6, "percentileForDuration: bar boundary → cumulative fraction");
  // A value below the smallest bar → 0.
  assertEq(SE.percentileForDuration(hist, 500000), 0, "percentileForDuration: below min → 0");
  // Interpolates within a bar (geometric midpoint of first bar ≈ 5% of total).
  const pGeoMid = SE.percentileForDuration(hist, Math.sqrt(1000000 * 2000000));
  assert(Math.abs(pGeoMid - 100 / 6) < 1e-6, "percentileForDuration: geometric interpolation within a bar");
  // Empty / invalid inputs.
  assertEq(SE.percentileForDuration([], 5), null, "percentileForDuration: no histogram → null");
  assertEq(SE.percentileForDuration(hist, null), null, "percentileForDuration: null duration → null");

  // fmtPercentile formatting bands.
  assertEq(SE.fmtPercentile(null), "—", "fmtPercentile: null → dash");
  assertEq(SE.fmtPercentile(50), "p50", "fmtPercentile: mid → pNN");
  assertEq(SE.fmtPercentile(99.9), "p99.9", "fmtPercentile: high tail keeps one decimal");
  assertEq(SE.fmtPercentile(99.99), "p99.99", "fmtPercentile: extreme tail keeps two decimals");
}

// ── collectExemplarAttributeKeys / exemplarAttrValue ──
{
  const exemplars = [
    { attributes: [{ key: "request_id", value: "r1" }, { key: "status_code", value: "500" }] },
    { attributes: [{ key: "status_code", value: "200" }, { key: "region", value: "pdx" }] },
    { attributes: [] },
    {},
  ];
  assertEq(
    SE.collectExemplarAttributeKeys(exemplars).join(","),
    "request_id,status_code,region",
    "collectExemplarAttributeKeys: union in first-seen order, no dupes",
  );
  assertEq(SE.collectExemplarAttributeKeys([]).length, 0, "collectExemplarAttributeKeys: empty → none");
  assertEq(SE.exemplarAttrValue(exemplars[0], "status_code"), "500", "exemplarAttrValue: present");
  assertEq(SE.exemplarAttrValue(exemplars[0], "region"), null, "exemplarAttrValue: absent key → null");
  assertEq(SE.exemplarAttrValue({}, "x"), null, "exemplarAttrValue: no attributes → null");
}

// ── columnIsDegenerate (auto-hide uniform columns) ──
{
  const rows = [{ h: "a" }, { h: "a" }, { h: "b" }];
  const same = [{ h: "a" }, { h: "a" }, { h: "a" }];
  const empty = [{}, { h: "" }, { h: null }];
  const get = (r) => r.h;
  assertEq(SE.columnIsDegenerate(same, get), true, "columnIsDegenerate: all same → true");
  assertEq(SE.columnIsDegenerate(empty, get), true, "columnIsDegenerate: all empty/missing → true");
  assertEq(SE.columnIsDegenerate(rows, get), false, "columnIsDegenerate: differing values → false");
  // Single row can't establish uniformity (a 1-row table keeps its columns).
  assertEq(SE.columnIsDegenerate([{ h: "a" }], get), false, "columnIsDegenerate: single row → false");
  assertEq(SE.columnIsDegenerate([], get), false, "columnIsDegenerate: empty rows → false");
  // Numeric vs string that stringify equal are treated as equal.
  assertEq(SE.columnIsDegenerate([{ h: 500 }, { h: "500" }], get), true, "columnIsDegenerate: 500 == '500'");
  // A mix of empty and non-empty is NOT degenerate.
  assertEq(SE.columnIsDegenerate([{ h: "a" }, {}], get), false, "columnIsDegenerate: empty + value → false");
}
// ── TIME_CATEGORIES ──

// ── mergeSelectedExemplarSnapshot ──
{
  const current = [
    { span_type_uid: "selected", count: 100, p99_ns: 900, exemplars: [{ elapsed_ns: 900 }] },
    { span_type_uid: "other", count: 50, exemplars: [{ elapsed_ns: 500 }] },
  ];
  const incomingPartial = [
    {
      span_type_uid: "selected",
      count: 3,
      p99_ns: 30,
      exemplars: [{ elapsed_ns: 25 }],
      selected_duration_count: 3,
    },
  ];
  const merged = SE.mergeSelectedExemplarSnapshot(current, incomingPartial, "selected");
  assert(merged.matched, "mergeSelectedExemplarSnapshot: reports selected type match");
  assertEq(merged.spanTypes.length, 2, "mergeSelectedExemplarSnapshot: preserves catalog rows");
  assertEq(merged.spanTypes[0].count, 100, "mergeSelectedExemplarSnapshot: preserves complete count");
  assertEq(merged.spanTypes[0].p99_ns, 900, "mergeSelectedExemplarSnapshot: preserves complete percentiles");
  assertEq(merged.spanTypes[0].exemplars[0].elapsed_ns, 25, "mergeSelectedExemplarSnapshot: patches exemplars");
  assertEq(merged.spanTypes[0].selected_duration_count, 3, "mergeSelectedExemplarSnapshot: patches exact count");
  assert(merged.spanTypes[1] === current[1], "mergeSelectedExemplarSnapshot: leaves unrelated type untouched");

  const absent = SE.mergeSelectedExemplarSnapshot(current, incomingPartial, "missing");
  assert(!absent.matched, "mergeSelectedExemplarSnapshot: reports absent selected type");
  assert(absent.spanTypes === current, "mergeSelectedExemplarSnapshot: retains current catalog when absent");
}

// ── classifyExemplarSnapshot ──
{
  const partial = SE.classifyExemplarSnapshot("catalog", "partial", "catalog");
  assert(partial.preview, "classifyExemplarSnapshot: previews a subset targeting the catalog");
  assert(!partial.complete, "classifyExemplarSnapshot: subset is not cache-complete");

  const complete = SE.classifyExemplarSnapshot("catalog", "catalog", "catalog");
  assert(complete.preview, "classifyExemplarSnapshot: exact set remains visible");
  assert(complete.complete, "classifyExemplarSnapshot: exact current set completes refresh");

  const unrelated = SE.classifyExemplarSnapshot("catalog", "other", "other-target");
  assert(!unrelated.preview, "classifyExemplarSnapshot: rejects an unrelated target set");
  assert(!unrelated.complete, "classifyExemplarSnapshot: unrelated set cannot complete refresh");
}

// ── completeExemplarRefresh ──
{
  const catalog = [
    { span_type_uid: "selected", count: 100 },
    { span_type_uid: "other", count: 50 },
  ];
  const coverage = { files_folded: 100, samples_folded: 150 };
  const complete = SE.completeExemplarRefresh(catalog, coverage, true);
  assert(complete.spanTypes === catalog, "completeExemplarRefresh: preserves full catalog");
  assert(complete.coverage === coverage, "completeExemplarRefresh: preserves catalog coverage");
  assert(!complete.pending, "completeExemplarRefresh: clears pending after adopted patch");

  const incomplete = SE.completeExemplarRefresh(catalog, coverage, false);
  assert(incomplete.spanTypes === catalog, "completeExemplarRefresh: interrupted refresh preserves catalog");
  assert(incomplete.coverage === coverage, "completeExemplarRefresh: interrupted refresh preserves coverage");
  assert(incomplete.pending, "completeExemplarRefresh: interrupted refresh stays pending");
}

// ── exemplarRequestMatches ──
{
  assert(
    SE.exemplarRequestMatches("A", "10:20", "A", "10:20"),
    "exemplarRequestMatches: accepts exact initiating type and bounds",
  );
  assert(
    !SE.exemplarRequestMatches("A", "10:20", "B", ":"),
    "exemplarRequestMatches: rejects response after selecting another type",
  );
  assert(
    !SE.exemplarRequestMatches("A", "10:20", "A", ":"),
    "exemplarRequestMatches: rejects late bounded response after A→B→A returns to global scope",
  );
}

// ── sameSpanCatalogStatistics ──
{
  const complete = [
    { span_type_uid: "a", count: 10, histogram: [{ lo_ns: 1, hi_ns: 2, count: 10 }], exemplars: [{ elapsed_ns: 2 }] },
  ];
  const exemplarOnlyChange = [
    { span_type_uid: "a", count: 10, histogram: [{ lo_ns: 1, hi_ns: 2, count: 10 }], exemplars: [{ elapsed_ns: 1 }], selected_duration_count: 4 },
  ];
  assert(
    SE.sameSpanCatalogStatistics(complete, exemplarOnlyChange),
    "sameSpanCatalogStatistics: ignores query-scoped exemplar fields",
  );
  const refined = [
    { span_type_uid: "a", count: 11, histogram: [{ lo_ns: 1, hi_ns: 2, count: 11 }], exemplars: [] },
  ];
  assert(
    !SE.sameSpanCatalogStatistics(complete, refined),
    "sameSpanCatalogStatistics: detects refined catalog changes",
  );
}
assertEq(SE.TIME_CATEGORIES.length, 5, "TIME_CATEGORIES: five categories");
assertEq(SE.TIME_CATEGORIES[0].key, "on_cpu", "TIME_CATEGORIES: first is on_cpu");
assertEq(SE.TIME_CATEGORIES[4].key, "unknown", "TIME_CATEGORIES: last is unknown");

// ── Attribute filters ──
{
  // parse: split on first '=', drop malformed (no '=' or empty key)
  const parsed = SE.parseAttrFilterParams([
    "status_code=500",
    "url=/a?b=c",
    "noequals",
    "=emptykey",
  ]);
  assertEq(parsed.length, 2, "parseAttrFilterParams: keeps only well-formed entries");
  assertEq(parsed[0].key, "status_code", "parseAttrFilterParams: key");
  assertEq(parsed[0].value, "500", "parseAttrFilterParams: value");
  assertEq(parsed[1].value, "/a?b=c", "parseAttrFilterParams: value may contain '='");

  // round-trip through format
  assertEq(
    JSON.stringify(SE.formatAttrFilterParams(parsed)),
    JSON.stringify(["status_code=500", "url=/a?b=c"]),
    "formatAttrFilterParams: round-trips key=value",
  );

  // has / add / remove are pure and dedupe
  assert(SE.hasAttrFilter(parsed, "status_code", "500"), "hasAttrFilter: present");
  assert(!SE.hasAttrFilter(parsed, "status_code", "200"), "hasAttrFilter: absent value");
  const added = SE.addAttrFilter(parsed, "host", "h1");
  assertEq(added.length, 3, "addAttrFilter: appends new filter");
  assertEq(
    SE.addAttrFilter(parsed, "status_code", "500").length,
    parsed.length,
    "addAttrFilter: no duplicate for existing pair",
  );
  const removed = SE.removeAttrFilter(added, "status_code", "500");
  assertEq(removed.length, 2, "removeAttrFilter: drops the matching pair");
  assert(
    !SE.hasAttrFilter(removed, "status_code", "500"),
    "removeAttrFilter: pair gone afterwards",
  );
  // Original array is not mutated (pure helpers).
  assertEq(parsed.length, 2, "attr filter helpers do not mutate input");
}

console.log(`\n${failed === 0 ? "All tests passed" : `${failed} test(s) FAILED`} (${passed} passed)`);
process.exit(failed === 0 ? 0 : 1);
