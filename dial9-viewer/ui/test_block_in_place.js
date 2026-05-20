#!/usr/bin/env node
// test_block_in_place.js — unit tests for block-in-place gap detection
// in trace_parser.js. See docs/adr/0001 and docs/adr/0002.
"use strict";

const {
  EVENT_TYPES,
  OFF_WORKER_WORKER_ID,
  deriveBlockInPlaceGaps,
} = require("./trace_parser.js");

let passed = 0;
let failed = 0;

function pass(msg) {
  console.log(`✓ ${msg}`);
  passed++;
}

function fail(msg) {
  console.log(`✗ ${msg}`);
  failed++;
}

function assertEq(actual, expected, label) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a === e) {
    pass(label);
  } else {
    fail(`${label}\n    expected: ${e}\n    actual:   ${a}`);
  }
}

function assert(cond, label) {
  if (cond) pass(label);
  else fail(label);
}

// Helpers to build synthetic events.
function unpark(t, w, tid) {
  return {
    eventType: EVENT_TYPES.WorkerUnpark,
    timestamp: t,
    workerId: w,
    tid,
    cpuTime: 0,
    schedWait: 0,
    localQueue: 0,
    globalQueue: 0,
    taskId: 0,
    spawnLocId: null,
    spawnLoc: null,
  };
}

function park(t, w, tid) {
  return {
    eventType: EVENT_TYPES.WorkerPark,
    timestamp: t,
    workerId: w,
    tid,
    cpuTime: 0,
    schedWait: 0,
    localQueue: 0,
    globalQueue: 0,
    taskId: 0,
    spawnLocId: null,
    spawnLoc: null,
  };
}

function sample(t, tid, wireWorkerId) {
  return {
    timestamp: t,
    workerId: wireWorkerId,
    tid,
    source: 0,
    callchain: [],
    cpu: null,
  };
}

// Test 1 — No block_in_place. Same tid throughout, no gaps detected.
function test_no_gap_straight_run() {
  console.log("\n— Test 1: no block_in_place");
  const events = [
    unpark(10, 0, 42),
    park(20, 0, 42),
    unpark(30, 0, 42),
    park(40, 0, 42),
  ];
  const samples = [
    sample(15, 42, 0), // during active
    sample(25, 42, 0), // while parked (wire still says W0)
    sample(35, 42, 0), // during second active
  ];
  const gaps = deriveBlockInPlaceGaps(events, samples);
  assertEq(gaps, [], "no gaps detected");
  // No gap → no rewrite. workerId stays as wire value (0).
  assertEq(samples.map((s) => s.workerId), [0, 0, 0],
    "no samples rewritten");
}

// Test 2 — Single block_in_place handoff.
function test_single_handoff() {
  console.log("\n— Test 2: single block_in_place");
  const events = [
    unpark(10, 0, 42),    // tid 42 takes worker 0
    park(50, 0, 99),      // park on tid 99 — gap detected, [10, 50)
    unpark(60, 0, 99),    // tid 99 now confirmed
    park(70, 0, 99),
  ];
  const samples = [
    sample(20, 42, 0),  // in gap, on fromTid
    sample(30, 99, 255), // in gap, on toTid (wire said unknown — blocking)
    sample(65, 99, 0),  // outside gap, after gap closed
    sample(20, 77, 255), // unrelated tid in gap window — should NOT be rewritten
  ];
  const gaps = deriveBlockInPlaceGaps(events, samples);
  assertEq(gaps, [{
    workerId: 0, fromTid: 42, toTid: 99, startNs: 10, endNs: 50,
  }], "one gap detected");
  assertEq(samples[0].workerId, OFF_WORKER_WORKER_ID,
    "fromTid sample in gap rewritten to off-worker");
  assertEq(samples[1].workerId, OFF_WORKER_WORKER_ID,
    "toTid sample in gap rewritten to off-worker (unchanged but still 255)");
  assertEq(samples[2].workerId, 0,
    "post-gap sample on toTid keeps wire workerId");
  assertEq(samples[3].workerId, 255,
    "unrelated tid in gap window is unchanged (still 255)");
}

// Test 3 — Sample at gap boundary. [start, end) is half-open.
function test_gap_boundary() {
  console.log("\n— Test 3: sample at gap boundaries");
  const events = [
    unpark(10, 0, 42),
    park(50, 0, 99),
  ];
  const samples = [
    sample(10, 42, 0),  // exactly at startNs — IN gap
    sample(50, 99, 0),  // exactly at endNs — NOT in gap (half-open)
    sample(49, 99, 0),  // just inside — IN gap
  ];
  deriveBlockInPlaceGaps(events, samples);
  assertEq(samples[0].workerId, OFF_WORKER_WORKER_ID,
    "sample at gap startNs is in gap");
  assertEq(samples[1].workerId, 0,
    "sample at gap endNs is NOT in gap (half-open)");
  assertEq(samples[2].workerId, OFF_WORKER_WORKER_ID,
    "sample 1ns before endNs is in gap");
}

// Test 4 — Repeated block_in_place; original tid returns.
function test_repeated_handoff() {
  console.log("\n— Test 4: repeated block_in_place, tid 42 returns");
  const events = [
    unpark(10, 0, 42),    // worker 0 = tid 42
    park(50, 0, 99),      // gap 1: [10, 50), 42→99
    unpark(60, 0, 42),    // gap 2: [50, 60), 99→42 (tid 42 reclaimed)
    park(70, 0, 42),
  ];
  const samples = [];
  const gaps = deriveBlockInPlaceGaps(events, samples);
  assertEq(gaps, [
    { workerId: 0, fromTid: 42, toTid: 99, startNs: 10, endNs: 50 },
    { workerId: 0, fromTid: 99, toTid: 42, startNs: 50, endNs: 60 },
  ], "two gaps detected");
}

// Test 5 — Multiple workers, one block_in_place.
function test_two_workers_one_handoff() {
  console.log("\n— Test 5: two workers, only W0 handoffs");
  const events = [
    unpark(10, 0, 42),
    unpark(15, 1, 77),
    park(50, 0, 99),     // W0 gap
    park(60, 1, 77),     // W1 normal
  ];
  const samples = [];
  const gaps = deriveBlockInPlaceGaps(events, samples);
  assertEq(gaps, [{
    workerId: 0, fromTid: 42, toTid: 99, startNs: 10, endNs: 50,
  }], "only W0's gap detected");
}

// Test 6 — Open binding at trace end (no closing park).
function test_open_at_end() {
  console.log("\n— Test 6: open binding at trace end");
  const events = [
    unpark(10, 0, 42),
    // No park before end of trace.
  ];
  const samples = [
    sample(50, 42, 0),  // during open binding
  ];
  const gaps = deriveBlockInPlaceGaps(events, samples);
  assertEq(gaps, [], "no gap detected (no closing park)");
  assertEq(samples[0].workerId, 0, "sample retains wire workerId");
}

// Test 7 — Open binding at trace start (park-first, no preceding unpark).
function test_open_at_start() {
  console.log("\n— Test 7: park without preceding unpark");
  const events = [
    park(50, 0, 42),    // first ever event for W0 is a park
    unpark(60, 0, 42),  // re-unpark same tid: no gap
    park(70, 0, 42),
  ];
  const samples = [
    sample(20, 42, 0),  // before any park/unpark
  ];
  const gaps = deriveBlockInPlaceGaps(events, samples);
  assertEq(gaps, [], "no gap detected (single tid throughout)");
  assertEq(samples[0].workerId, 0, "sample retains wire workerId");
}

// Test 8 — Long block_in_place chain across multiple tids.
function test_chain_42_99_77() {
  console.log("\n— Test 8: chain 42 → 99 → 77");
  const events = [
    unpark(10, 0, 42),
    park(50, 0, 99),     // gap 1: 42→99
    unpark(60, 0, 99),
    park(70, 0, 77),     // gap 2: 99→77
    unpark(80, 0, 77),
  ];
  const samples = [];
  const gaps = deriveBlockInPlaceGaps(events, samples);
  assertEq(gaps, [
    { workerId: 0, fromTid: 42, toTid: 99, startNs: 10, endNs: 50 },
    { workerId: 0, fromTid: 99, toTid: 77, startNs: 60, endNs: 70 },
  ], "two gaps in chain");
}

// Test 9 — Sample on a tid that never appears in park/unpark.
function test_unrelated_tid() {
  console.log("\n— Test 9: sample on unrelated tid");
  const events = [
    unpark(10, 0, 42),
    park(50, 0, 99),
  ];
  const samples = [
    sample(20, 555, 255),  // tid 555 never seen as worker
    sample(30, 555, 0),    // tid 555 with wire workerId 0 (wrong, kept as-is)
  ];
  deriveBlockInPlaceGaps(events, samples);
  assertEq(samples[0].workerId, 255,
    "unrelated tid keeps its wire workerId");
  assertEq(samples[1].workerId, 0,
    "unrelated tid keeps wire workerId even if it was wrongly populated");
}

// Bonus: old-format trace (no `tid` on park/unpark) → no-op.
function test_no_tid_no_op() {
  console.log("\n— Bonus: old-format trace without tid is no-op");
  const events = [
    { ...unpark(10, 0, 42), tid: undefined },
    { ...park(50, 0, 99), tid: undefined },
  ];
  const samples = [
    sample(20, 42, 0),
    sample(30, 99, 0),
  ];
  const gaps = deriveBlockInPlaceGaps(events, samples);
  assertEq(gaps, [], "no gaps without tid");
  assertEq(samples.map((s) => s.workerId), [0, 0],
    "no samples rewritten without tid");
}

function main() {
  test_no_gap_straight_run();
  test_single_handoff();
  test_gap_boundary();
  test_repeated_handoff();
  test_two_workers_one_handoff();
  test_open_at_end();
  test_open_at_start();
  test_chain_42_99_77();
  test_unrelated_tid();
  test_no_tid_no_op();

  // Integration test: decode a real trace with block_in_place gaps.
  test_real_trace_integration();

  console.log(`\n${passed} passed, ${failed} failed`);
  process.exit(failed > 0 ? 1 : 0);
}

async function test_real_trace_integration() {
  console.log("\n— Integration: real block_in_place trace");
  const fs = require("fs");
  const path = require("path");
  const { parseTrace } = require("./trace_parser.js");

  const tracePath = path.join(__dirname, "test-traces", "block_in_place.bin");
  if (!fs.existsSync(tracePath)) {
    fail(`Trace file not found: ${tracePath}`);
    return;
  }

  const trace = await parseTrace(fs.readFileSync(tracePath));
  assert(trace.blockInPlaceGaps.length > 0,
    `detected ${trace.blockInPlaceGaps.length} block_in_place gap(s)`);

  // Each gap should have valid fields.
  for (const g of trace.blockInPlaceGaps) {
    assert(g.workerId != null && g.fromTid > 0 && g.toTid > 0,
      `gap has valid workerId=${g.workerId}, fromTid=${g.fromTid}, toTid=${g.toTid}`);
    assert(g.fromTid !== g.toTid,
      `gap fromTid !== toTid (${g.fromTid} !== ${g.toTid})`);
    assert(g.endNs > g.startNs,
      `gap endNs > startNs (${g.endNs} > ${g.startNs})`);
  }
}

// Run — integration test is async so we need to await it.
(async () => {
  test_no_gap_straight_run();
  test_single_handoff();
  test_gap_boundary();
  test_repeated_handoff();
  test_two_workers_one_handoff();
  test_open_at_end();
  test_open_at_start();
  test_chain_42_99_77();
  test_unrelated_tid();
  test_no_tid_no_op();
  await test_real_trace_integration();

  console.log(`\n${passed} passed, ${failed} failed`);
  process.exit(failed > 0 ? 1 : 0);
})();
