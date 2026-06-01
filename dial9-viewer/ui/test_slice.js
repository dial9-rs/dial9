#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const { sliceTrace } = require("../../dial9-trace-format/js/slice.js");
const { parseTrace } = require("./trace_parser.js");

async function main() {
  const tracePath = path.join(__dirname, "demo-trace.bin");
  if (!fs.existsSync(tracePath)) {
    console.error(`Trace file not found: ${tracePath}`);
    process.exit(1);
  }

  let failures = 0;
  function fail(msg) {
    console.log(`✗ ${msg}`);
    failures++;
  }
  function pass(msg) {
    console.log(`✓ ${msg}`);
  }

  const input = fs.readFileSync(tracePath);

  // Parse the full trace for reference
  const full = await parseTrace(input);
  const fullEventCount = full.events.length + full.cpuSamples.length + full.customEvents.length;
  console.log(`Full trace: ${fullEventCount} total events, minTs=${full.minTs}, maxTs=${full.maxTs}`);

  // ── Test 1: Slicing with full range returns all events ──
  {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: full.minTs.toString(), endNs: full.maxTs.toString() }
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    if (slicedEventCount === fullEventCount) {
      pass(`Full-range slice preserves all ${fullEventCount} events`);
    } else {
      fail(`Full-range slice: expected ${fullEventCount} events, got ${slicedEventCount}`);
    }
  }

  // ── Test 2: Sub-range slice yields strictly smaller file ──
  {
    const midTs = Math.floor((full.minTs + full.maxTs) / 2);
    const sliced = sliceTrace(input, {
      timeRange: { startNs: full.minTs.toString(), endNs: midTs.toString() }
    });
    if (sliced.length < input.length) {
      pass(`Sub-range slice is smaller: ${sliced.length} < ${input.length} bytes`);
    } else {
      fail(`Sub-range slice should be smaller: ${sliced.length} vs ${input.length}`);
    }

    // All events in the sliced output should be within range
    const parsed = await parseTrace(sliced);
    let outOfRange = 0;
    for (const e of parsed.events) {
      if (e.timestamp < full.minTs || e.timestamp > midTs) outOfRange++;
    }
    for (const s of parsed.cpuSamples) {
      if (s.timestamp < full.minTs || s.timestamp > midTs) outOfRange++;
    }
    for (const c of parsed.customEvents) {
      if (c.timestamp < full.minTs || c.timestamp > midTs) outOfRange++;
    }
    if (outOfRange === 0) {
      pass("All events in sub-range slice are within the specified time range");
    } else {
      fail(`${outOfRange} events outside the specified time range`);
    }
  }

  // ── Test 3: Sliced output is round-trip parseable ──
  {
    const quarterTs = Math.floor(full.minTs + (full.maxTs - full.minTs) / 4);
    const threeQuarterTs = Math.floor(full.minTs + 3 * (full.maxTs - full.minTs) / 4);
    const sliced = sliceTrace(input, {
      timeRange: { startNs: quarterTs.toString(), endNs: threeQuarterTs.toString() }
    });
    try {
      const parsed = await parseTrace(sliced);
      const count = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
      if (count > 0 && count < fullEventCount) {
        pass(`Quarter-range slice is parseable with ${count} events (< ${fullEventCount})`);
      } else if (count === 0) {
        fail("Quarter-range slice has 0 events");
      } else {
        fail(`Quarter-range slice has ${count} events, expected fewer than ${fullEventCount}`);
      }
    } catch (err) {
      fail(`Quarter-range slice failed to parse: ${err.message}`);
    }
  }

  // ── Test 4: No-filter returns identical content ──
  {
    const sliced = sliceTrace(input);
    if (sliced.length === input.length) {
      pass("No-filter slice returns same size as input");
    } else {
      // Might differ slightly due to gzip handling, but content should parse the same
      const parsed = await parseTrace(sliced);
      const count = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
      if (count === fullEventCount) {
        pass(`No-filter slice preserves all ${fullEventCount} events (size differs due to decompression)`);
      } else {
        fail(`No-filter slice: expected ${fullEventCount} events, got ${count}`);
      }
    }
  }

  // ── Test 5: Handles gzipped input ──
  // demo-trace.bin is already gzipped, so just pass it directly
  {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: full.minTs.toString(), endNs: full.maxTs.toString() }
    });
    // Verify the output is valid (not gzipped — slicer always outputs raw)
    if (sliced[0] === 0x54 && sliced[1] === 0x52 && sliced[2] === 0x43) {
      pass("Gzipped input decompressed and sliced to raw output");
    } else {
      fail(`Unexpected output header: 0x${sliced[0].toString(16)} 0x${sliced[1].toString(16)}`);
    }
  }

  // ── Test 6: Relative slice with [0, durationFull] returns all events ──
  // Note: relative mode anchors from the first event's timestamp (findMinTs),
  // which may differ slightly from parseTrace's minTs (global minimum).
  // Use a generous end to ensure we capture everything.
  {
    const duration = full.maxTs - full.minTs;
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "0", endNs: (duration + 1000000000).toString() },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    if (slicedEventCount === fullEventCount) {
      pass(`Relative full-range slice preserves all ${fullEventCount} events`);
    } else {
      fail(`Relative full-range slice: expected ${fullEventCount} events, got ${slicedEventCount}`);
    }
  }

  // ── Test 7: Relative slice with [0, halfDuration] returns ~half the events ──
  {
    const halfDuration = Math.floor((full.maxTs - full.minTs) / 2);
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "0", endNs: halfDuration.toString() },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    // All events should be within [minTs, minTs + halfDuration]
    let outOfRange = 0;
    for (const e of parsed.events) {
      if (e.timestamp < full.minTs || e.timestamp > full.minTs + halfDuration) outOfRange++;
    }
    for (const s of parsed.cpuSamples) {
      if (s.timestamp < full.minTs || s.timestamp > full.minTs + halfDuration) outOfRange++;
    }
    for (const c of parsed.customEvents) {
      if (c.timestamp < full.minTs || c.timestamp > full.minTs + halfDuration) outOfRange++;
    }
    if (slicedEventCount > 0 && slicedEventCount < fullEventCount && outOfRange === 0) {
      pass(`Relative half-range slice: ${slicedEventCount} events, all within range`);
    } else if (slicedEventCount === 0) {
      fail("Relative half-range slice has 0 events");
    } else if (outOfRange > 0) {
      fail(`Relative half-range slice: ${outOfRange} events out of range`);
    } else {
      fail(`Relative half-range slice: expected fewer than ${fullEventCount}, got ${slicedEventCount}`);
    }
  }

  // ── Test 8: Relative slice [3.9s, 4.05s] on demo-trace produces non-empty slice ──
  {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "3900000000", endNs: "4050000000" },
      relative: true,
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    if (slicedEventCount > 0) {
      pass(`Relative [3.9s, 4.05s] slice has ${slicedEventCount} events`);
    } else {
      fail("Relative [3.9s, 4.05s] slice has 0 events (this is the foot-gun case)");
    }
  }

  // ── Test 9: Without --relative, small values produce 0-event slice (backward compat) ──
  {
    const sliced = sliceTrace(input, {
      timeRange: { startNs: "3900000000", endNs: "4050000000" },
      // no relative: true — absolute mode
    });
    const parsed = await parseTrace(sliced);
    const slicedEventCount = parsed.events.length + parsed.cpuSamples.length + parsed.customEvents.length;
    if (slicedEventCount === 0) {
      pass("Absolute mode with small relative-looking values correctly produces 0 events");
    } else {
      fail(`Absolute mode with small values: expected 0 events, got ${slicedEventCount}`);
    }
  }

  console.log(`\n${failures === 0 ? "All tests passed" : `${failures} test(s) FAILED`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch(err => {
  console.error(err);
  process.exit(1);
});
