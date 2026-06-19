#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");
const { assert, testAsync, summarize } = require("./test_harness.js");
const { sliceTrace } = require("../../dial9-trace-format/js/slice.js");
const { parseTrace } = require("./trace_parser.js");

async function main() {
  const tracePath = path.join(__dirname, "demo-trace.bin");
  if (!fs.existsSync(tracePath)) {
    console.error(`Trace file not found: ${tracePath}`);
    process.exit(1);
  }

  const input = fs.readFileSync(tracePath);
  const rawInput = input[0] === 0x1f && input[1] === 0x8b
    ? zlib.gunzipSync(input)
    : input;

  // Parse the full trace for reference
  const full = await parseTrace(input);
  const sliceMinTs = full.recordMinTs ?? full.minTs;
  const sliceMaxTs = full.recordMaxTs ?? full.maxTs;
  const fullEventCount = full.events.length + full.cpuSamples.length + full.customEvents.length;
  console.log(`Full trace: ${fullEventCount} total events, minTs=${full.minTs}, maxTs=${full.maxTs}, recordMinTs=${sliceMinTs}, recordMaxTs=${sliceMaxTs}`);

  // ── Test 1: Slicing with full range returns all events ──
  await testAsync("Full-range slice preserves all events", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: sliceMinTs.toString(), endNs: sliceMaxTs.toString() }
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    assert.strictEqual(slicedEventCount, fullEventCount);
  });

  // ── Test 2: Sub-range slice yields strictly smaller file ──
  await testAsync("Sub-range slice is smaller and events are within range", async () => {
    const midTs = Math.floor((sliceMinTs + sliceMaxTs) / 2);
    const sliced = sliceTrace(input, {
      timeRange: { startNs: sliceMinTs.toString(), endNs: midTs.toString() }
    });
    assert.ok(sliced.length < rawInput.length, `${sliced.length} should be < ${rawInput.length}`);

    const parsed = await parseTrace(sliced);
    for (const e of parsed.events) {
      assert.ok(e.timestamp >= sliceMinTs && e.timestamp <= midTs, `event out of range: ${e.timestamp}`);
    }
    for (const s of parsed.cpuSamples) {
      assert.ok(s.timestamp >= sliceMinTs && s.timestamp <= midTs, `cpuSample out of range: ${s.timestamp}`);
    }
    for (const c of parsed.customEvents) {
      assert.ok(c.timestamp >= sliceMinTs && c.timestamp <= midTs, `customEvent out of range: ${c.timestamp}`);
    }
  });

  // ── Test 3: Sliced output is round-trip parseable ──
  await testAsync("Quarter-range slice is parseable with fewer events", async () => {
    const quarterTs = Math.floor(sliceMinTs + (sliceMaxTs - sliceMinTs) / 4);
    const threeQuarterTs = Math.floor(sliceMinTs + 3 * (sliceMaxTs - sliceMinTs) / 4);
    const sliced = sliceTrace(input, {
      timeRange: { startNs: quarterTs.toString(), endNs: threeQuarterTs.toString() }
    });
    const parsed = await parseTrace(sliced);
    const count = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    assert.ok(count > 0, "should have some events");
    assert.ok(count < fullEventCount, `${count} should be < ${fullEventCount}`);
  });

  // ── Test 4: No-filter returns identical content ──
  await testAsync("No-filter slice preserves all events", async () => {
    const sliced = sliceTrace(input);
    const parsed = await parseTrace(sliced);
    const count = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    assert.strictEqual(count, fullEventCount);
  });

  // ── Test 5: Handles gzipped input ──
  await testAsync("Gzipped input decompressed and sliced to raw output", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: sliceMinTs.toString(), endNs: sliceMaxTs.toString() }
    });
    // Output is raw (starts with TRC magic)
    assert.strictEqual(sliced[0], 0x54);
    assert.strictEqual(sliced[1], 0x52);
    assert.strictEqual(sliced[2], 0x43);
  });

  // ── Test 6: Relative slice with [0, durationFull] returns all events ──
  // Note: relative mode anchors from the first timestamped event in stream order
  // (findMinTs), which may differ from parseTrace's minTs (the global minimum
  // across all events). Use a generous end to ensure we capture everything.
  await testAsync("Relative full-range slice preserves all events", async () => {
    const duration = sliceMaxTs - sliceMinTs;
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "0", endNs: (duration + 1000000000).toString() },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    assert.strictEqual(slicedEventCount, fullEventCount);
  });

  // ── Test 7: Relative slice with [0, halfDuration] returns ~half the events ──
  await testAsync("Relative half-range slice: subset of events, all within range", async () => {
    const halfDuration = Math.floor((sliceMaxTs - sliceMinTs) / 2);
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "0", endNs: halfDuration.toString() },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    assert.ok(slicedEventCount > 0, "should have events");
    assert.ok(slicedEventCount < fullEventCount, `${slicedEventCount} should be < ${fullEventCount}`);
    for (const e of parsed.events) {
      assert.ok(e.timestamp >= sliceMinTs && e.timestamp <= sliceMinTs + halfDuration, `event out of range: ${e.timestamp}`);
    }
    for (const s of parsed.cpuSamples) {
      assert.ok(s.timestamp >= sliceMinTs && s.timestamp <= sliceMinTs + halfDuration, `cpuSample out of range`);
    }
    for (const c of parsed.customEvents) {
      assert.ok(c.timestamp >= sliceMinTs && c.timestamp <= sliceMinTs + halfDuration, `customEvent out of range`);
    }
  });

  // ── Test 8: Relative slice [3.9s, 4.05s] on demo-trace produces non-empty slice ──
  await testAsync("Relative [3.9s, 4.05s] slice has events", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "3900000000", endNs: "4050000000" },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    assert.ok(slicedEventCount > 0, "should have events (this is the foot-gun case)");
  });

  // ── Test 9: Without --relative, small values produce 0-event slice (backward compat) ──
  await testAsync("Absolute mode with small relative-looking values produces 0 events", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "3900000000", endNs: "4050000000" },
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    assert.strictEqual(slicedEventCount, 0);
  });

  // ── Test 10: Relative [0, 100ms] slice preserves all SymbolTableEntry events ──
  await testAsync("Relative [0, 100ms] slice preserves all symbols", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "0", endNs: "100000000" },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    assert.ok(parsed.callframeSymbols.size > 0, "should have symbols");
    assert.strictEqual(parsed.callframeSymbols.size, full.callframeSymbols.size);
  });

  // ── Test 11: Half-range slice: time filter works AND symbols preserved ──
  await testAsync("Half-range slice: time filter works AND symbols preserved", async () => {
    const halfDuration = Math.floor((sliceMaxTs - sliceMinTs) / 2);
    const sliceEnd = sliceMinTs + halfDuration;
    const sliced = sliceTrace(input, {
      timeRange: { startNs: sliceMinTs.toString(), endNs: sliceEnd.toString() }
    });
    const parsed = await parseTrace(sliced);
    for (const e of parsed.events) {
      assert.ok(e.timestamp >= sliceMinTs && e.timestamp <= sliceEnd, `event out of range`);
    }
    for (const s of parsed.cpuSamples) {
      assert.ok(s.timestamp >= sliceMinTs && s.timestamp <= sliceEnd, `cpuSample out of range`);
    }
    for (const c of parsed.customEvents) {
      assert.ok(c.timestamp >= sliceMinTs && c.timestamp <= sliceEnd, `customEvent out of range`);
    }
    assert.strictEqual(parsed.callframeSymbols.size, full.callframeSymbols.size);
  });

  // ── Test 12: Full-range absolute slice preserves all events + symbols ──
  await testAsync("Full-range absolute slice: all events and symbols preserved", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: sliceMinTs.toString(), endNs: sliceMaxTs.toString() }
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    assert.strictEqual(slicedEventCount, fullEventCount);
    assert.strictEqual(parsed.callframeSymbols.size, full.callframeSymbols.size);
    assert.strictEqual(parsed.taskSpawnTimes.size, full.taskSpawnTimes.size);
    assert.strictEqual(parsed.taskTerminateTimes.size, full.taskTerminateTimes.size);
  });

  // ── Test 13: Relative [3.9s, 4.05s] slice has symbols for flamegraphs ──
  await testAsync("Relative [3.9s, 4.05s] slice preserves all symbols", async () => {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "3900000000", endNs: "4050000000" },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    assert.strictEqual(parsed.callframeSymbols.size, full.callframeSymbols.size);
  });

  summarize();
}

main().catch(err => {
  console.error(err);
  process.exit(1);
});
