#!/usr/bin/env node
"use strict";

// Unit tests for findTaskLifecycleInversions: the task-lifecycle consistency
// check must tolerate tiny cross-worker CLOCK_MONOTONIC skew (a clock artifact
// on virtualized CI runners) while still flagging genuine corruption.
const assert = require("assert");
const { findTaskLifecycleInversions } = require("./test_trace_integrity.js");

let passed = 0;
function check(name, fn) {
  fn();
  console.log(`✓ ${name}`);
  passed++;
}

check("normal lifecycle has no inversions", () => {
  const spawn = new Map([
    [1, 100],
    [2, 200],
  ]);
  const term = new Map([
    [1, 150],
    [2, 250],
  ]);
  const { tolerated, gross } = findTaskLifecycleInversions(spawn, term);
  assert.strictEqual(tolerated.length, 0);
  assert.strictEqual(gross.length, 0);
});

check("sub-millisecond cross-worker skew is tolerated, not gross", () => {
  // Short-lived task: spawns on one core at T, terminates on another core whose
  // monotonic clock lags by 500ns -> terminate recorded 500ns *before* spawn.
  const spawn = new Map([[1, 1_000_000_000]]);
  const term = new Map([[1, 1_000_000_000 - 500]]);
  const { tolerated, gross } = findTaskLifecycleInversions(spawn, term);
  assert.strictEqual(gross.length, 0, "sub-ms skew must not be flagged gross");
  assert.strictEqual(tolerated.length, 1, "sub-ms skew should be tolerated");
  assert.strictEqual(tolerated[0].taskId, 1);
  assert.strictEqual(tolerated[0].delta, 500);
});

check("inversion exactly at the tolerance boundary is tolerated", () => {
  const tol = 1_000_000;
  const spawn = new Map([[7, 5_000_000_000]]);
  const term = new Map([[7, 5_000_000_000 - tol]]); // delta == tolerance
  const { tolerated, gross } = findTaskLifecycleInversions(spawn, term, tol);
  assert.strictEqual(gross.length, 0);
  assert.strictEqual(tolerated.length, 1);
});

check("gross inversion (2s before spawn) is flagged", () => {
  const spawn = new Map([[1, 5_000_000_000]]);
  const term = new Map([[1, 3_000_000_000]]); // 2s before spawn
  const { tolerated, gross } = findTaskLifecycleInversions(spawn, term);
  assert.strictEqual(gross.length, 1, "2s inversion must be gross");
  assert.strictEqual(gross[0].taskId, 1);
  assert.strictEqual(gross[0].delta, 2_000_000_000);
  assert.strictEqual(tolerated.length, 0);
});

check("terminate without spawn is ignored", () => {
  const spawn = new Map([[1, 100]]);
  const term = new Map([[2, 50]]); // different id; id 1 never terminates
  const { tolerated, gross } = findTaskLifecycleInversions(spawn, term);
  assert.strictEqual(tolerated.length, 0);
  assert.strictEqual(gross.length, 0);
});

check("custom tolerance is honored", () => {
  const spawn = new Map([[1, 1_000_000]]);
  const term = new Map([[1, 1_000_000 - 5000]]); // 5us before
  // With a 1us tolerance, 5us is gross.
  const { gross } = findTaskLifecycleInversions(spawn, term, 1000);
  assert.strictEqual(gross.length, 1);
});

console.log(`\n✓ All ${passed} task-lifecycle checks passed!`);
