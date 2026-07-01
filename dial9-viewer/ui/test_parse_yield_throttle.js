#!/usr/bin/env node
"use strict";

// Regression test for issue #595: the buffered trace parser (parseTraceBuffer,
// used for multi-trace `trace=` loads — a single trace streams instead) must
// throttle its expensive macrotask yield by WALL-CLOCK time, not by bytes.
//
// The bug: parseTraceBuffer used to `await setTimeout(0)` once per 100KB of
// decoded bytes. Each setTimeout(0) is clamped to ~4ms in browsers and forces a
// repaint, so a 10MB trace fired ~108 yields/repaints and parsed far slower
// than the single-trace streaming path (which already throttled by wall-clock).
//
// parseTraceStream already moved off the byte cadence; this asserts the buffered
// path matches that behavior. We mock the clock so the test is fully
// deterministic (no dependence on how fast the host actually parses):
//   • frozen clock  → ZERO macrotask yields, even though onProgress fires on
//     every 100KB milestone. (The old byte-cadence code yielded on each.)
//   • fast clock    → the parser still yields, so the spinner can repaint.

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");
const { assert, testAsync, summarize } = require("./test_harness.js");
const { parseTrace } = require("./trace_parser.js");

// Run `fn` with global setTimeout counted (delay-0 macrotask yields only) and
// performance.now driven by `clock()`. Restores both afterward.
async function withInstrumentedClock(clock, fn) {
  const realSetTimeout = global.setTimeout;
  const realPerformance = global.performance;
  let yields = 0;
  global.setTimeout = function (cb, delay, ...rest) {
    if (delay === 0 || delay == null) yields++;
    return realSetTimeout(cb, delay, ...rest);
  };
  // parseTraceBuffer's nowMs closure reads `performance.now()` at call time, so
  // swapping the whole global picks up our clock. (performance.now itself is a
  // read-only property, so we can't reassign just the method.)
  global.performance = { now: clock };
  try {
    await fn();
  } finally {
    global.setTimeout = realSetTimeout;
    global.performance = realPerformance;
  }
  return yields;
}

async function main() {
  const tracePath = path.join(__dirname, "demo-trace.bin");
  if (!fs.existsSync(tracePath)) {
    console.error(`Trace file not found: ${tracePath}`);
    process.exit(1);
  }
  const fileBytes = fs.readFileSync(tracePath);
  const raw = Uint8Array.from(
    fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
      ? zlib.gunzipSync(fileBytes)
      : Buffer.from(fileBytes),
  );

  // Sanity: the demo is big enough to cross many 100KB progress milestones, so
  // a byte-cadence yield would fire many times. Otherwise the test is vacuous.
  const milestones = Math.floor(raw.length / (100 * 1024));
  assert.ok(
    milestones >= 20,
    `demo trace spans ${milestones} progress milestones (need >= 20 for a meaningful test)`,
  );

  // Reference: events from an un-instrumented parse, to prove the yield change
  // doesn't alter decode output.
  const reference = await parseTrace(raw);
  assert.ok(reference.events.length > 0, "reference has events");

  // ── Frozen clock: the wall-clock throttle must suppress ALL macrotask yields
  //    while onProgress still fires on every 100KB milestone. The pre-#595
  //    byte-cadence code yielded once per milestone here. ──
  await testAsync("frozen clock: progress fires but zero macrotask yields", async () => {
    let progressFires = 0;
    const yields = await withInstrumentedClock(
      () => 1000, // never advances
      async () => {
        const t = await parseTrace(raw, {
          onParseProgress: () => {
            progressFires++;
          },
        });
        assert.strictEqual(
          t.events.length,
          reference.events.length,
          "throttling must not change decoded event count",
        );
      },
    );
    assert.ok(
      progressFires >= 20,
      `onProgress should fire on each 100KB milestone (got ${progressFires})`,
    );
    assert.strictEqual(
      yields,
      0,
      `frozen clock must yield 0 times, got ${yields} (byte-cadence regression: would be ~${progressFires})`,
    );
  });

  // ── Fast clock: every milestone is >= PAINT_INTERVAL_MS apart, so the parser
  //    still hands the thread back so the spinner can repaint. ──
  await testAsync("fast clock: parser still yields so the spinner repaints", async () => {
    let progressFires = 0;
    let t = 0;
    const yields = await withInstrumentedClock(
      () => (t += 1000), // +1s per read >> 200ms paint interval
      async () => {
        await parseTrace(raw, {
          onParseProgress: () => {
            progressFires++;
          },
        });
      },
    );
    assert.ok(yields >= 1, `fast clock should still yield at least once (got ${yields})`);
    // Yields are clamped to the paint cadence: never more than one per progress
    // milestone (and with a real clock, far fewer).
    assert.ok(
      yields <= progressFires,
      `yields (${yields}) must not exceed progress milestones (${progressFires})`,
    );
  });

  // ── No progress callback: no yields at all (used by Node directory parsing
  //    and tests, where there is no spinner to tick). ──
  await testAsync("no onParseProgress: parser never yields", async () => {
    const yields = await withInstrumentedClock(
      () => 1000,
      async () => {
        await parseTrace(raw); // no onParseProgress
      },
    );
    assert.strictEqual(yields, 0, `parse without onParseProgress must not yield (got ${yields})`);
  });

  summarize();
}

main();
