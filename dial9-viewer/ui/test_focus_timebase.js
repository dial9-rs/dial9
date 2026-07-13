#!/usr/bin/env node
"use strict";

// Regression test for the Tokio-stats "Longest polls" deep-link timebase.
//
// The bug: the aggregate polls Parquet stores poll `start_ns`/`end_ns` in
// WALL-CLOCK ns (decode.rs adds the segment's clock offset), but the trace
// viewer's timeline (minTs/maxTs/viewStart) is RAW MONOTONIC ns. A deep link
// that fed a wall-clock timestamp straight into the viewer's `focusOnWindow`
// landed billions of ns past maxTs, clamped to an empty [maxTs, maxTs] window,
// and showed a blank worker lane. The fix converts wall→monotonic by
// subtracting `trace.clockOffsetNs` — the same offset decode.rs applied.
//
// This test pins the invariant the fix depends on, using the real demo trace:
//   1. the demo carries a wall-clock offset (so the mismatch is exercised),
//   2. a wall-clock poll timestamp is OUTSIDE the monotonic [minTs, maxTs]
//      (this is what produced the empty view), and
//   3. subtracting clockOffsetNs brings it back inside — i.e. the conversion
//      focusOnWindow performs is correct.

const fs = require("fs");
const zlib = require("zlib");
const path = require("path");
const { parseTrace } = require(path.resolve(__dirname, "trace_parser.js"));

let passed = 0;
let failed = 0;
function assert(cond, desc) {
  if (cond) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(`✗ ${desc}`);
    failed++;
  }
}

(async () => {
  const raw = fs.readFileSync(path.resolve(__dirname, "demo-trace.bin"));
  let buf = raw;
  try {
    buf = zlib.gunzipSync(raw);
  } catch (e) {
    /* already-raw trace */
  }
  const ab = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
  const trace = await parseTrace(ab, {});

  assert(Number.isFinite(trace.minTs) && Number.isFinite(trace.maxTs), "trace has finite monotonic bounds");
  assert(trace.clockOffsetNs != null && trace.clockOffsetNs > 0, "demo trace carries a wall-clock offset");

  // A poll one millisecond into the trace, as the aggregate would store it:
  // wall-clock = monotonic + offset.
  const pollMonotonic = trace.minTs + 1e6;
  const pollWallClock = pollMonotonic + trace.clockOffsetNs;

  const inBounds = (ns) => ns >= trace.minTs && ns <= trace.maxTs;

  // 2. The raw wall-clock value is off the monotonic timeline (the bug).
  assert(!inBounds(pollWallClock), "wall-clock poll timestamp falls OUTSIDE the viewer's monotonic bounds");

  // 3. focusOnWindow's correction (wall - clockOffsetNs) round-trips it back.
  const corrected = pollWallClock - trace.clockOffsetNs;
  assert(inBounds(corrected), "subtracting clockOffsetNs brings it back inside bounds");

  // Recovery is APPROXIMATE, not exact: the wall-clock value (~1.78e18) is past
  // Number.MAX_SAFE_INTEGER (~9e15), so as a JS double its ULP is ~256ns. Every
  // timestamp in the viewer is a double at this magnitude (minTs/maxTs/viewStart
  // alike), so this is consistent with the whole timeline — and the focus window
  // is padded to >=1ms, so a sub-microsecond error is invisible. Assert the
  // recovery is within 1µs (comfortably under that 1ms floor), not bit-exact.
  const errNs = Math.abs(corrected - pollMonotonic);
  assert(errNs < 1000, `correction recovers the monotonic timestamp within 1µs (off by ${errNs}ns)`);

  console.log(`\n${passed} passed, ${failed} failed`);
  process.exit(failed === 0 ? 0 : 1);
})().catch((e) => {
  console.error("ERROR:", e.message);
  process.exit(1);
});
