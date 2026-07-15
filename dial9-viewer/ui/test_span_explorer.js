#!/usr/bin/env node
"use strict";

// Unit tests for the Span Explorer helpers in span_explorer.js: duration
// formatting, catalog sorting, histogram normalization/layout/brush, time
// composition, URL deep-linking, flamegraph URL generation, and raw trace
// catalog building. All DOM-free.

const fs = require("fs");
const path = require("path");
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
    bucket: "my-bucket",
    region: "us-west-2",
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
  assertEq(encoded.get("bucket"), "my-bucket", "encodeState: bucket");
  assertEq(encoded.get("aws_region"), "us-west-2", "encodeState: region");
  assertEq(encoded.get("span_type_uid"), state.span_type_uid, "encodeState: span_type_uid");
  assertEq(encoded.get("min_span_ns"), "1000000", "encodeState: min_span_ns");
  assertEq(encoded.getAll("host").length, 2, "encodeState: hosts repeatable");

  const decoded = SE.decodeSpanExplorerState(encoded);
  assertEq(decoded.bucket, "my-bucket", "decodeState: bucket");
  assertEq(decoded.span_type_uid, state.span_type_uid, "decodeState: span_type_uid");
  assertEq(decoded.min_span_ns, 1000000, "decodeState: min_span_ns");
  assertEq(decoded.hosts.length, 2, "decodeState: hosts");
}

// ── flamegraphUrl ──
{
  const state = {
    bucket: "bkt",
    region: null,
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
  assert(cpuUrl.includes("source=cpu"), "flamegraphUrl: on_cpu → source=cpu");
  assert(cpuUrl.includes("span_type_uid=aabb"), "flamegraphUrl: carries span_type_uid");
  assert(cpuUrl.includes("min_span_ns=1000"), "flamegraphUrl: carries min_span_ns");
  assert(cpuUrl.startsWith("flamegraph.html?"), "flamegraphUrl: points at flamegraph.html");

  const blockUrl = SE.flamegraphUrl(state, "blocking");
  assert(blockUrl.includes("source=sched"), "flamegraphUrl: blocking → source=sched");
}

// ── TIME_CATEGORIES ──
assertEq(SE.TIME_CATEGORIES.length, 5, "TIME_CATEGORIES: five categories");
assertEq(SE.TIME_CATEGORIES[0].key, "on_cpu", "TIME_CATEGORIES: first is on_cpu");
assertEq(SE.TIME_CATEGORIES[4].key, "unknown", "TIME_CATEGORIES: last is unknown");

// ── parseSpanEventName ──
{
  const r1 = SE.parseSpanEventName("SpanEnter:metrics_service::buffer::flush_to_ddb:examples/metrics-service/src/buffer.rs:55");
  assert(r1 != null, "parseSpanEventName: parses valid SpanEnter");
  assertEq(r1.target, "metrics_service::buffer", "parseSpanEventName: target");
  assertEq(r1.name, "flush_to_ddb", "parseSpanEventName: name");
  assertEq(r1.file, "examples/metrics-service/src/buffer.rs", "parseSpanEventName: file");
  assertEq(r1.line, 55, "parseSpanEventName: line");

  const r2 = SE.parseSpanEventName("SpanExit:metrics_service::routes::record_metric:examples/metrics-service/src/routes.rs:26");
  assert(r2 != null, "parseSpanEventName: parses SpanExit");
  assertEq(r2.name, "record_metric", "parseSpanEventName: SpanExit name");

  const r3 = SE.parseSpanEventName("SpanCloseEvent");
  assertEq(r3, null, "parseSpanEventName: SpanClose → null");

  const r4 = SE.parseSpanEventName("SpanEnter__CustomStruct");
  assertEq(r4, null, "parseSpanEventName: SpanEnter__ (no colon format) → null");

  const r5 = SE.parseSpanEventName("SomeOtherEvent");
  assertEq(r5, null, "parseSpanEventName: unrelated event → null");
}

// ── buildLogHistogram ──
{
  const durations = [100, 200, 300, 1000, 2000, 5000, 10000];
  const hist = SE.buildLogHistogram(durations, 100, 10000);
  assert(hist.length > 0, "buildLogHistogram: produces bins");
  const totalCount = hist.reduce((s, b) => s + b.count, 0);
  assertEq(totalCount, 7, "buildLogHistogram: total count matches input");
  assert(hist[0].lo_ns <= 100, "buildLogHistogram: first bin starts at or below min");
  assert(hist[hist.length - 1].hi_ns >= 10000, "buildLogHistogram: last bin covers max");
  for (const b of hist) {
    assert(b.hi_ns > b.lo_ns, "buildLogHistogram: every bin has hi > lo");
    assert(b.count >= 0, "buildLogHistogram: no negative counts");
  }
}

{
  // Edge case: single duration
  const hist = SE.buildLogHistogram([500], 500, 500);
  assert(hist.length > 0, "buildLogHistogram: single value produces bins");
  const totalCount = hist.reduce((s, b) => s + b.count, 0);
  assertEq(totalCount, 1, "buildLogHistogram: single value total count = 1");
}

// ── buildSpanCatalog (synthetic) ──
{
  const customEvents = [
    { name: "SpanEnter:myapp::handler::do_stuff:src/handler.rs:10", timestamp: 100, fields: { span_id: "1", span_name: "do_stuff", worker_id: 0 } },
    { name: "SpanExit:myapp::handler::do_stuff:src/handler.rs:10", timestamp: 200, fields: { span_id: "1", span_name: "do_stuff", worker_id: 0 } },
    { name: "SpanCloseEvent", timestamp: 201, fields: { span_id: "1" } },
    { name: "SpanEnter:myapp::handler::do_stuff:src/handler.rs:10", timestamp: 300, fields: { span_id: "2", span_name: "do_stuff", worker_id: 0 } },
    { name: "SpanExit:myapp::handler::do_stuff:src/handler.rs:10", timestamp: 500, fields: { span_id: "2", span_name: "do_stuff", worker_id: 0 } },
    { name: "SpanCloseEvent", timestamp: 501, fields: { span_id: "2" } },
    { name: "SpanEnter:myapp::db::query:src/db.rs:42", timestamp: 150, fields: { span_id: "3", span_name: "query", worker_id: 1 } },
    { name: "SpanExit:myapp::db::query:src/db.rs:42", timestamp: 350, fields: { span_id: "3", span_name: "query", worker_id: 1 } },
    { name: "SpanCloseEvent", timestamp: 351, fields: { span_id: "3" } },
  ];

  // Use the buildSpanData from trace_analysis if available, otherwise mock
  let buildSpanData;
  try {
    buildSpanData = require("./trace_analysis.js").buildSpanData;
  } catch (e) {
    // Fallback: provide minimal implementation
    buildSpanData = null;
  }

  if (buildSpanData) {
    const { allSpans } = buildSpanData(customEvents);
    const catalog = SE.buildSpanCatalog(allSpans, customEvents);

    assertEq(catalog.length, 2, "buildSpanCatalog: 2 span types from synthetic data");

    const doStuff = catalog.find(c => c.name === "do_stuff");
    assert(doStuff != null, "buildSpanCatalog: found do_stuff type");
    assertEq(doStuff.count, 2, "buildSpanCatalog: do_stuff has 2 instances");
    assertEq(doStuff.target, "myapp::handler", "buildSpanCatalog: do_stuff target");
    assertEq(doStuff.callsite_file, "src/handler.rs", "buildSpanCatalog: do_stuff file");
    assertEq(doStuff.callsite_line, 10, "buildSpanCatalog: do_stuff line");
    assertEq(doStuff.kind, "tracing", "buildSpanCatalog: kind is tracing");
    assert(doStuff.p50_ns > 0, "buildSpanCatalog: do_stuff p50 > 0");
    assert(doStuff.histogram.length > 0, "buildSpanCatalog: do_stuff has histogram");
    assertEq(doStuff.details_complete_count, 0, "buildSpanCatalog: quality 0 complete");
    assertEq(doStuff.partial_count, 2, "buildSpanCatalog: quality 2 partial");

    const query = catalog.find(c => c.name === "query");
    assert(query != null, "buildSpanCatalog: found query type");
    assertEq(query.count, 1, "buildSpanCatalog: query has 1 instance");
    assertEq(query.callsite_line, 42, "buildSpanCatalog: query line = 42");

    // Verify composition is null (all-Unknown fallback)
    assertEq(doStuff.composition, null, "buildSpanCatalog: composition is null (raw mode)");
    // Verify computeTimeComposition falls back correctly
    const comp = SE.computeTimeComposition(doStuff);
    assertEq(comp.categories.length, 1, "buildSpanCatalog+composition: single Unknown");
    assertEq(comp.categories[0].key, "unknown", "buildSpanCatalog+composition: key is unknown");
  }
}

// ── Demo trace integration test ──
async function testDemoTrace() {
  const tracePath = path.join(__dirname, "demo-trace.bin");

// ── Browser dependency order regression ──
{
  const html = fs.readFileSync(path.join(__dirname, "span_explorer.html"), "utf8");
  const decoderPos = html.indexOf('<script src="decode.js"></script>');
  const parserPos = html.indexOf('<script src="trace_parser.js"></script>');
  assert(decoderPos >= 0, "span_explorer.html: loads decode.js");
  assert(parserPos >= 0, "span_explorer.html: loads trace_parser.js");
  assert(decoderPos < parserPos, "span_explorer.html: loads TraceDecoder before trace_parser.js");
}
  if (!fs.existsSync(tracePath)) {
    console.log("· demo-trace.bin absent — skipping demo trace integration tests");
    return;
  }

  const { parseTrace } = require("./trace_parser.js");
  const { buildSpanData } = require("./trace_analysis.js");

  const buf = fs.readFileSync(tracePath);
  const parsed = await parseTrace(buf);

  assert(parsed.customEvents.length > 0, "demo trace: has customEvents");

  const { allSpans } = buildSpanData(parsed.customEvents);
  assert(allSpans.length > 0, "demo trace: buildSpanData produces spans");

  const catalog = SE.buildSpanCatalog(allSpans, parsed.customEvents);
  assert(catalog.length > 0, "demo trace: buildSpanCatalog produces span types");

  // Known span types from the demo metrics-service trace
  const names = catalog.map(c => c.name);
  assert(names.includes("record_metric"), "demo trace: contains record_metric span type");
  assert(names.includes("query_metric"), "demo trace: contains query_metric span type");
  assert(names.includes("flush_to_ddb"), "demo trace: contains flush_to_ddb span type");

  // Verify counts are nonzero
  for (const entry of catalog) {
    assert(entry.count > 0, `demo trace: ${entry.name} count > 0 (got ${entry.count})`);
    assert(entry.p50_ns >= 0, `demo trace: ${entry.name} p50_ns >= 0`);
    assert(entry.max_ns >= entry.p50_ns, `demo trace: ${entry.name} max >= p50`);
    assert(entry.histogram.length > 0, `demo trace: ${entry.name} has histogram bins`);

    // Histogram total count must equal entry count
    const histTotal = entry.histogram.reduce((s, b) => s + b.count, 0);
    assertEq(histTotal, entry.count, `demo trace: ${entry.name} histogram total matches count`);

    // All bins must be valid
    for (const bin of entry.histogram) {
      assert(bin.hi_ns > bin.lo_ns, `demo trace: ${entry.name} bin hi > lo`);
      assert(bin.count >= 0, `demo trace: ${entry.name} bin count >= 0`);
    }
  }

  // Verify callsite info is populated
  const recordMetric = catalog.find(c => c.name === "record_metric");
  assert(recordMetric.target != null, "demo trace: record_metric has target");
  assert(recordMetric.callsite_file != null, "demo trace: record_metric has callsite_file");
  assert(recordMetric.callsite_line > 0, "demo trace: record_metric has callsite_line");
  assert(recordMetric.target.includes("metrics_service"), "demo trace: record_metric target contains metrics_service");

  // Verify quality fields (raw mode = all partial)
  for (const entry of catalog) {
    assertEq(entry.details_complete_count, 0, `demo trace: ${entry.name} details_complete = 0 (raw)`);
    assertEq(entry.partial_count, entry.count, `demo trace: ${entry.name} partial = count (raw)`);
    const q = SE.spanTypeQuality(entry);
    assertEq(q, 0, `demo trace: ${entry.name} quality = 0 (all partial)`);
  }

  // Verify computeTimeComposition on catalog entries falls back to Unknown
  for (const entry of catalog) {
    const comp = SE.computeTimeComposition(entry);
    assert(comp.total_ns > 0, `demo trace: ${entry.name} composition total_ns > 0`);
    assertEq(comp.categories.length, 1, `demo trace: ${entry.name} composition is single Unknown`);
    assertEq(comp.categories[0].key, "unknown", `demo trace: ${entry.name} composition key = unknown`);
  }
}

// Run async tests then print summary
testDemoTrace().then(() => {
  console.log(`\n${failed === 0 ? "All tests passed" : `${failed} test(s) FAILED`} (${passed} passed)`);
  process.exit(failed === 0 ? 0 : 1);
}).catch(err => {
  console.error("Test runner error:", err);
  process.exit(1);
});
