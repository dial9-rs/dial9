#!/usr/bin/env node
"use strict";

// Tests for embed.html — verifies the underlying rendering logic works
// without a DOM. We test that:
// 1. embed.html exists and has the expected structure
// 2. The JS modules it depends on can process the demo trace for both views
// 3. Flamegraph filtering produces samples in a time range
// 4. Worker spans build correctly for timeline rendering

const fs = require("fs");
const path = require("path");
const { parseTrace, EVENT_TYPES } = require("./trace_parser.js");
const TraceAnalysis = require("./trace_analysis.js");
const FlamegraphRenderer = require("./flamegraph.js");

async function main() {
  let failures = 0;
  function fail(msg) { console.log(`✗ ${msg}`); failures++; }
  function pass(msg) { console.log(`✓ ${msg}`); }

  // ── Test 1: embed.html exists and has required structure ──
  const embedPath = path.join(__dirname, "embed.html");
  if (!fs.existsSync(embedPath)) {
    fail("embed.html does not exist");
    process.exit(1);
  }
  const html = fs.readFileSync(embedPath, "utf8");
  const requiredElements = [
    'src="decode.js"',
    'src="trace_parser.js"',
    'src="trace_analysis.js"',
    'src="flamegraph.js"',
    'id="container"',
    'id="loading"',
    'id="error"',
    'params.get("trace")',
    'params.get("view")',
    'params.get("start")',
    'params.get("end")',
    'params.get("height")',
  ];
  let structureOk = true;
  for (const el of requiredElements) {
    if (!html.includes(el)) {
      fail(`embed.html missing: ${el}`);
      structureOk = false;
    }
  }
  if (structureOk) pass("embed.html has all required structure");

  // ── Test 2: Supports both flamegraph and timeline views ──
  if (html.includes('"flamegraph"') && html.includes('"timeline"')) {
    pass("embed.html supports flamegraph and timeline views");
  } else {
    fail("embed.html missing view support");
  }

  // ── Test 3: Load demo trace and verify flamegraph data ──
  const tracePath = path.join(__dirname, "demo-trace.bin");
  if (!fs.existsSync(tracePath)) {
    fail("demo-trace.bin not found");
    process.exit(1);
  }

  const buf = fs.readFileSync(tracePath);
  const trace = await parseTrace(buf);

  // Flamegraph: filter CPU samples to a sub-range
  const midTs = Math.floor((trace.minTs + trace.maxTs) / 2);
  const filteredSamples = FlamegraphRenderer.filterCpuSamples(trace.cpuSamples, trace.minTs, midTs);
  if (filteredSamples.length > 0 && filteredSamples.length < trace.cpuSamples.length) {
    pass(`Flamegraph filter: ${filteredSamples.length} samples in first half (of ${trace.cpuSamples.length} total)`);
  } else if (filteredSamples.length === 0) {
    fail("Flamegraph filter returned 0 samples for first half");
  } else {
    fail(`Flamegraph filter: expected fewer than ${trace.cpuSamples.length}, got ${filteredSamples.length}`);
  }

  // ── Test 4: Build worker spans for timeline ──
  const wSet = new Set();
  const ET = EVENT_TYPES;
  trace.events.forEach(e => {
    if (e.eventType !== ET.QueueSample && e.eventType !== ET.WakeEvent) wSet.add(e.workerId);
  });
  const workerIds = [...wSet].sort((a, b) => a - b);
  const maxTs = trace.events.length > 0 ? trace.events[trace.events.length - 1].timestamp : 0;

  const spanResult = TraceAnalysis.buildWorkerSpans(trace.events, workerIds, maxTs, trace.blockInPlaceGaps);
  let hasPolls = false;
  let hasParks = false;
  for (const wid of workerIds) {
    const spans = spanResult.workerSpans[wid];
    if (spans && spans.polls && spans.polls.length > 0) hasPolls = true;
    if (spans && spans.parks && spans.parks.length > 0) hasParks = true;
  }
  if (hasPolls) pass(`Timeline: worker spans have polls (${workerIds.length} workers)`);
  else fail("Timeline: no poll spans found");
  if (hasParks) pass("Timeline: worker spans have parks");
  else fail("Timeline: no park spans found");

  // ── Test 5: pollHeatmapColorQuantized returns valid colors ──
  const color = TraceAnalysis.pollHeatmapColorQuantized(1000000); // 1ms
  if (color && color.startsWith("#") && color.length === 7) {
    pass(`pollHeatmapColorQuantized(1ms) = ${color}`);
  } else {
    fail(`pollHeatmapColorQuantized returned invalid color: ${color}`);
  }

  // ── Test 6: No CDN dependencies in embed.html ──
  if (!html.includes("cdn") && !html.includes("unpkg") && !html.includes("jsdelivr")) {
    pass("embed.html has no CDN dependencies");
  } else {
    fail("embed.html contains CDN references");
  }

  // ── Test 7: Works with file:// (no absolute URLs required) ──
  // All script srcs should be relative
  const scriptSrcs = html.match(/src="([^"]+)"/g) || [];
  const allRelative = scriptSrcs.every(s => !s.includes("http://") && !s.includes("https://"));
  if (allRelative) {
    pass("All script sources are relative paths (file:// compatible)");
  } else {
    fail("embed.html has absolute script URLs");
  }

  console.log(`\n${failures === 0 ? "All tests passed" : `${failures} test(s) FAILED`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch(err => {
  console.error(err);
  process.exit(1);
});
