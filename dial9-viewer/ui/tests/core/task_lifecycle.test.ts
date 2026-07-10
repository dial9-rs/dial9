// Unit tests for findTaskLifecycleInversions: the task-lifecycle consistency
// check must tolerate tiny cross-worker CLOCK_MONOTONIC skew (a clock artifact
// on virtualized CI runners) while still flagging genuine corruption.
//
// Migrated from test_task_lifecycle.js (T11). The function under test used to
// be required from test_trace_integrity.js; it now lives in
// helpers/task_lifecycle_inversions.ts (see the note there).

import { describe, it, expect } from "vitest";
import { findTaskLifecycleInversions } from "./helpers/task_lifecycle_inversions";

describe("findTaskLifecycleInversions", () => {
  it("normal lifecycle has no inversions", () => {
    const spawn = new Map([
      [1, 100],
      [2, 200],
    ]);
    const term = new Map([
      [1, 150],
      [2, 250],
    ]);
    const { tolerated, gross } = findTaskLifecycleInversions(spawn, term);
    expect(tolerated.length).toBe(0);
    expect(gross.length).toBe(0);
  });

  it("sub-millisecond cross-worker skew is tolerated, not gross", () => {
    // Short-lived task: spawns on one core at T, terminates on another core whose
    // monotonic clock lags by 500ns -> terminate recorded 500ns *before* spawn.
    const spawn = new Map([[1, 1_000_000_000]]);
    const term = new Map([[1, 1_000_000_000 - 500]]);
    const { tolerated, gross } = findTaskLifecycleInversions(spawn, term);
    expect(gross.length, "sub-ms skew must not be flagged gross").toBe(0);
    expect(tolerated.length, "sub-ms skew should be tolerated").toBe(1);
    expect(tolerated[0]!.taskId).toBe(1);
    expect(tolerated[0]!.delta).toBe(500);
  });

  it("inversion exactly at the tolerance boundary is tolerated", () => {
    const tol = 1_000_000;
    const spawn = new Map([[7, 5_000_000_000]]);
    const term = new Map([[7, 5_000_000_000 - tol]]); // delta == tolerance
    const { tolerated, gross } = findTaskLifecycleInversions(spawn, term, tol);
    expect(gross.length).toBe(0);
    expect(tolerated.length).toBe(1);
  });

  it("gross inversion (2s before spawn) is flagged", () => {
    const spawn = new Map([[1, 5_000_000_000]]);
    const term = new Map([[1, 3_000_000_000]]); // 2s before spawn
    const { tolerated, gross } = findTaskLifecycleInversions(spawn, term);
    expect(gross.length, "2s inversion must be gross").toBe(1);
    expect(gross[0]!.taskId).toBe(1);
    expect(gross[0]!.delta).toBe(2_000_000_000);
    expect(tolerated.length).toBe(0);
  });

  it("terminate without spawn is ignored", () => {
    const spawn = new Map([[1, 100]]);
    const term = new Map([[2, 50]]); // different id; id 1 never terminates
    const { tolerated, gross } = findTaskLifecycleInversions(spawn, term);
    expect(tolerated.length).toBe(0);
    expect(gross.length).toBe(0);
  });

  it("custom tolerance is honored", () => {
    const spawn = new Map([[1, 1_000_000]]);
    const term = new Map([[1, 1_000_000 - 5000]]); // 5us before
    // With a 1us tolerance, 5us is gross.
    const { gross } = findTaskLifecycleInversions(spawn, term, 1000);
    expect(gross.length).toBe(1);
  });
});
