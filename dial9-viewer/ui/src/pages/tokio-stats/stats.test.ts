// The single-period assertions use the recorded refine response; diff cases
// use synthetic two-period responses with hand-verifiable rates.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, it, expect } from "vitest";
import type {
  SpawnLocStats,
  TokioStatsResponse,
  WorkerStats,
} from "../../lib/trace/index.js";
import {
  buildDiffModel,
  buildHostRows,
  computeStats,
  singlePeriodRows,
  sortedWorkers,
} from "./stats.js";

const REFINE: TokioStatsResponse = JSON.parse(
  readFileSync(
    fileURLToPath(
      new URL("../../../tests/fixtures/aggregates/tokio-stats-refine.json", import.meta.url),
    ),
    "utf8",
  ),
);

const AXUM = "examples/metrics-service/src/axum_traced.rs:243:33";
const HYPER =
  "/usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/hyper-util-0.1.20/src/rt/tokio.rs:115:9";
const MAIN311 = "examples/metrics-service/src/main.rs:311:24";
const MAIN289 = "examples/metrics-service/src/main.rs:289:14";
const MAIN366 = "examples/metrics-service/src/main.rs:366:24";

describe("computeStats against the recorded refine fixture", () => {
  // Threshold 100us = the wire floor: every notable duration is above it, so
  // `long` per location equals its durations_ns length (its notable count).
  const s = computeStats(REFINE, 100000)!;

  it("carries total_polls through unchanged", () => {
    expect(s.total_polls).toBe(94212);
  });

  it("sums notable polls to 3379 across the five locations", () => {
    expect(s.totalLong).toBe(3379);
    expect(s.byLoc[AXUM]!.long).toBe(3319);
    expect(s.byLoc[HYPER]!.long).toBe(29);
    expect(s.byLoc[MAIN311]!.long).toBe(22);
    expect(s.byLoc[MAIN289]!.long).toBe(9);
    expect(s.byLoc[MAIN366]!.long).toBe(0); // empty durations
  });

  it("no off-CPU polls in the demo trace; on-CPU only at the class-1 location", () => {
    // The demo's longest poll is ~1ms (< the 10ms off-CPU confidence bound),
    // so class 0 never occurs (classes 1/2/3 only).
    expect(s.totalOffCpu).toBe(0);
    expect(s.totalOnCpu).toBeGreaterThan(0);
    // AXUM is the only location carrying class-1 (on-CPU) polls.
    expect(s.byLoc[AXUM]!.onCpu).toBe(s.totalOnCpu);
    expect(s.byLoc[HYPER]!.onCpu).toBe(0);
  });

  it("percentiles read the DESC-sorted durations: p99 >= p50, max is the head", () => {
    const m = s.byLoc[MAIN289]!; // durations [5895291, ...9 entries...], all class 3
    expect(m.max).toBe(5895291);
    expect(m.p99).toBe(5895291); // floor(9*0.01)=0 -> the head
    expect(m.p50).toBe(253000); // floor(9*0.5)=4
  });

  it("rate is long / (time_span/60e9), using the response's observed span", () => {
    const timeMinutes = REFINE.time_span_ns / 60e9;
    expect(s.rate).toBeCloseTo(3379 / timeMinutes, 6);
    expect(s.byLoc[MAIN289]!.rate).toBeCloseTo(9 / timeMinutes, 6);
  });

  it("single-period rows exclude long==0 locations, sorted by rate desc", () => {
    const rows = singlePeriodRows(s);
    expect(rows.map((r) => r.loc)).not.toContain(MAIN366);
    expect(rows).toHaveLength(4);
    expect(rows[0]!.loc).toBe(AXUM); // highest notable count -> highest rate
  });

  it("thresholding is a pure client re-filter: at 1ms only the 2 >=1ms polls survive", () => {
    const s1ms = computeStats(REFINE, 1e6)!;
    expect(s1ms.totalLong).toBe(2); // AXUM 1013250 + MAIN289 5895291
    expect(s1ms.byLoc[AXUM]!.long).toBe(1);
    expect(s1ms.byLoc[MAIN289]!.long).toBe(1);
    expect(s1ms.byLoc[MAIN289]!.max).toBe(5895291);
    expect(s1ms.byLoc[HYPER]!.long).toBe(0);
  });

  it("null in -> null out (an unloaded period)", () => {
    expect(computeStats(null, 100000)).toBeNull();
  });
});

// ── Diff model against synthetic two-period responses ──

function loc(spawn: string, durations: number[], classes: number[]): SpawnLocStats {
  return {
    spawn_loc: spawn,
    total_polls: durations.length,
    durations_ns: durations,
    classes,
    exemplars: [null, null, null, null],
  };
}
function resp(total: number, locs: SpawnLocStats[]): TokioStatsResponse {
  return { time_span_ns: 60e9, total_polls: total, bucket: "b", by_spawn_loc: locs };
}

describe("buildDiffModel", () => {
  // timeMinutes = 1, so rate == long. P1: reg@2, imp@5. P2: reg@5, imp@1, new@3.
  const p1 = computeStats(
    resp(7, [
      loc("reg", [200000, 200000], [1, 1]),
      loc("imp", [200000, 200000, 200000, 200000, 200000], [1, 1, 1, 1, 1]),
    ]),
    100000,
  );
  const p2 = computeStats(
    resp(9, [
      loc("reg", [200000, 200000, 200000, 200000, 200000], [1, 1, 1, 1, 1]),
      loc("imp", [200000], [1]),
      loc("new", [200000, 200000, 200000], [1, 1, 1]),
    ]),
    100000,
  );
  const model = buildDiffModel([p1, p2], 2);

  it("cards compare P1 total rate to P{last} with sign, arrow, and %", () => {
    expect(model.cards.firstRate).toBe(7);
    expect(model.cards.lastRate).toBe(9);
    expect(model.cards.arrow).toBe("↑");
    expect(model.cards.cls).toBe("bad"); // delta 2 > 0.5
    expect(model.cards.ratePctLabel).toBe("+29%"); // ((2/7)*100).toFixed(0)
    expect(model.cards.firstTotalPolls).toBe(7);
    expect(model.cards.lastTotalPolls).toBe(9);
  });

  it("regressions: delta > +0.1/min, worst first - new offenders overlap", () => {
    expect(model.regressions.map((r) => r.loc).sort()).toEqual(["new", "reg"]);
    const reg = model.regressions.find((r) => r.loc === "reg")!;
    expect(reg.delta).toBeCloseTo(3, 6);
    expect(reg.pct).toBe("150"); // ((3/2)*100).toFixed(0) - rendered raw (quirk)
  });

  it("new offenders: fRate==0 && lRate>0, pct 'new'", () => {
    expect(model.newOffenders.map((r) => r.loc)).toEqual(["new"]);
    expect(model.newOffenders[0]!.pct).toBe("new");
  });

  it("improvements: delta < -0.1/min, most-improved first", () => {
    expect(model.improvements.map((r) => r.loc)).toEqual(["imp"]);
    expect(model.improvements[0]!.delta).toBeCloseTo(-4, 6);
  });

  it("+∞% when P1's rate was 0 and P{last} > 0 (edge)", () => {
    const zero = computeStats(resp(0, [loc("x", [], [])]), 100000);
    const grew = computeStats(resp(3, [loc("x", [200000, 200000, 200000], [1, 1, 1])]), 100000);
    expect(buildDiffModel([zero, grew], 2).cards.ratePctLabel).toBe("+∞%");
  });

  it("throws when P1 is unloaded", () => {
    // buildDiffModel requires the first and last periods to be loaded.
    expect(() => buildDiffModel([null, p2, p2], 3)).toThrow();
  });
});


describe("buildHostRows (Worker activity grouping)", () => {
  function worker(over: Partial<WorkerStats> = {}): WorkerStats {
    return {
      worker_id: 0,
      host: "h1",
      total_polls: 10,
      busy_ns: 1_000,
      span_ns: 10_000,
      busy_pct: 10,
      notable_polls: 1,
      worst_poll_ns: 5_000,
      ...over,
    };
  }
  const workerResp = (workers: WorkerStats[]): TokioStatsResponse => ({
    time_span_ns: 60e9,
    total_polls: 100,
    bucket: "b",
    by_spawn_loc: [],
    worker_activity: workers,
  });

  it("returns [] when the response carries no worker activity", () => {
    expect(buildHostRows(undefined, "busyPct", true)).toEqual([]);
    expect(buildHostRows(workerResp([]), "busyPct", true)).toEqual([]);
  });

  it("groups workers by host and sums poll counts", () => {
    const rows = buildHostRows(
      workerResp([
        worker({ host: "h1", worker_id: 0, total_polls: 10, notable_polls: 2 }),
        worker({ host: "h1", worker_id: 1, total_polls: 5, notable_polls: 1 }),
        worker({ host: "h2", worker_id: 0, total_polls: 7, notable_polls: 3 }),
      ]),
      "host",
      false,
    );
    expect(rows.map((r) => r.host)).toEqual(["h1", "h2"]);
    expect(rows[0]!.totalPolls).toBe(15);
    expect(rows[0]!.notablePolls).toBe(3);
    expect(rows[1]!.totalPolls).toBe(7);
  });

  it("host busyness POOLS workers (Σ busy / Σ span), not a mean of percentages", () => {
    // A sparsely-observed worker at 100% and a heavily-observed one at 1%:
    // pooled = (100 + 100) / (100 + 10_000) = ~1.98%, whereas averaging the
    // per-worker percentages would report a misleading ~50%.
    const rows = buildHostRows(
      workerResp([
        worker({ worker_id: 0, busy_ns: 100, span_ns: 100, busy_pct: 100 }),
        worker({ worker_id: 1, busy_ns: 100, span_ns: 10_000, busy_pct: 1 }),
      ]),
      "busyPct",
      true,
    );
    expect(rows[0]!.busyPct).toBeCloseTo((200 / 10_100) * 100, 6);
    expect(rows[0]!.busyPct).toBeLessThan(3);
  });

  it("reports active vs configured worker counts (max id + 1)", () => {
    // Workers 0 and 3 observed -> 2 active, 4 configured.
    const rows = buildHostRows(
      workerResp([worker({ worker_id: 0 }), worker({ worker_id: 3 })]),
      "busyPct",
      true,
    );
    expect(rows[0]!.activeWorkers).toBe(2);
    expect(rows[0]!.numWorkers).toBe(4);
  });

  it("worstPollNs is the max across the host's workers", () => {
    const rows = buildHostRows(
      workerResp([
        worker({ worker_id: 0, worst_poll_ns: 5_000 }),
        worker({ worker_id: 1, worst_poll_ns: 9_000 }),
      ]),
      "busyPct",
      true,
    );
    expect(rows[0]!.worstPollNs).toBe(9_000);
  });

  it("sorts by the requested key in both directions", () => {
    const data = workerResp([
      worker({ host: "a", total_polls: 5 }),
      worker({ host: "b", total_polls: 50 }),
    ]);
    expect(buildHostRows(data, "totalPolls", true).map((r) => r.host)).toEqual(["b", "a"]);
    expect(buildHostRows(data, "totalPolls", false).map((r) => r.host)).toEqual(["a", "b"]);
    // Host is a string key, so it compares lexically rather than numerically.
    expect(buildHostRows(data, "host", false).map((r) => r.host)).toEqual(["a", "b"]);
  });

  it("falls back to '(unknown)' for a blank host", () => {
    expect(buildHostRows(workerResp([worker({ host: "" })]), "busyPct", true)[0]!.host).toBe(
      "(unknown)",
    );
  });

  it("sortedWorkers orders a host's workers busiest-first without mutating", () => {
    const ws = [worker({ worker_id: 0, busy_pct: 1 }), worker({ worker_id: 1, busy_pct: 9 })];
    const row = buildHostRows(workerResp(ws), "busyPct", true)[0]!;
    expect(sortedWorkers(row).map((w) => w.worker_id)).toEqual([1, 0]);
    // The row keeps its original order (sortedWorkers copies).
    expect(row.workers.map((w) => w.worker_id)).toEqual([0, 1]);
  });
});
