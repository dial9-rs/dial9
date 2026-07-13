// Behavioral-differ target: computeStats + the diff math must reproduce the
// legacy page's numbers EXACTLY from the same cached response. The
// single-period assertions run against the recorded refine fixture (the exact
// wire the dev seed folds); the diff assertions run against synthetic
// two-period responses with hand-verifiable rates.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, it, expect } from "vitest";
import type { SpawnLocStats, TokioStatsResponse } from "../../lib/trace/index.js";
import {
  buildDiffModel,
  computeStats,
  shouldStopRefining,
  singlePeriodRows,
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

describe("computeStats against the recorded refine fixture (E1-E4)", () => {
  // Threshold 100us = the wire floor: every notable duration is above it, so
  // `long` per location equals its durations_ns length (its notable count).
  const s = computeStats(REFINE, 100000)!;

  it("carries total_polls through unchanged (F1)", () => {
    expect(s.total_polls).toBe(94212);
  });

  it("sums notable polls to 3379 across the five locations (E1)", () => {
    expect(s.totalLong).toBe(3379);
    expect(s.byLoc[AXUM]!.long).toBe(3319);
    expect(s.byLoc[HYPER]!.long).toBe(29);
    expect(s.byLoc[MAIN311]!.long).toBe(22);
    expect(s.byLoc[MAIN289]!.long).toBe(9);
    expect(s.byLoc[MAIN366]!.long).toBe(0); // empty durations
  });

  it("no off-CPU polls in the demo trace; on-CPU only at the class-1 location (E2)", () => {
    // The demo's longest poll is ~1ms (< the 10ms off-CPU confidence bound),
    // so class 0 never occurs (classes 1/2/3 only).
    expect(s.totalOffCpu).toBe(0);
    expect(s.totalOnCpu).toBeGreaterThan(0);
    // AXUM is the only location carrying class-1 (on-CPU) polls.
    expect(s.byLoc[AXUM]!.onCpu).toBe(s.totalOnCpu);
    expect(s.byLoc[HYPER]!.onCpu).toBe(0);
  });

  it("percentiles read the DESC-sorted durations: p99 >= p50, max is the head (E4)", () => {
    const m = s.byLoc[MAIN289]!; // durations [5895291, ...9 entries...], all class 3
    expect(m.max).toBe(5895291);
    expect(m.p99).toBe(5895291); // floor(9*0.01)=0 -> the head
    expect(m.p50).toBe(253000); // floor(9*0.5)=4
  });

  it("rate is long / (time_span/60e9), using the response's observed span (E3)", () => {
    const timeMinutes = REFINE.time_span_ns / 60e9;
    expect(s.rate).toBeCloseTo(3379 / timeMinutes, 6);
    expect(s.byLoc[MAIN289]!.rate).toBeCloseTo(9 / timeMinutes, 6);
  });

  it("single-period rows exclude long==0 locations, sorted by rate desc (F2/F3)", () => {
    const rows = singlePeriodRows(s);
    expect(rows.map((r) => r.loc)).not.toContain(MAIN366);
    expect(rows).toHaveLength(4);
    expect(rows[0]!.loc).toBe(AXUM); // highest notable count -> highest rate
  });

  it("thresholding is a pure client re-filter: at 1ms only the 2 >=1ms polls survive (C4)", () => {
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

describe("buildDiffModel (G3-G9)", () => {
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

  it("cards compare P1 total rate to P{last} with sign, arrow, and % (G4)", () => {
    expect(model.cards.firstRate).toBe(7);
    expect(model.cards.lastRate).toBe(9);
    expect(model.cards.arrow).toBe("↑");
    expect(model.cards.cls).toBe("bad"); // delta 2 > 0.5
    expect(model.cards.ratePctLabel).toBe("+29%"); // ((2/7)*100).toFixed(0)
    expect(model.cards.firstTotalPolls).toBe(7);
    expect(model.cards.lastTotalPolls).toBe(9);
  });

  it("regressions: delta > +0.1/min, worst first (G5) - new offenders overlap (G6)", () => {
    expect(model.regressions.map((r) => r.loc).sort()).toEqual(["new", "reg"]);
    const reg = model.regressions.find((r) => r.loc === "reg")!;
    expect(reg.delta).toBeCloseTo(3, 6);
    expect(reg.pct).toBe("150"); // ((3/2)*100).toFixed(0) - rendered raw (quirk)
  });

  it("new offenders: fRate==0 && lRate>0, pct 'new' (G6/G9)", () => {
    expect(model.newOffenders.map((r) => r.loc)).toEqual(["new"]);
    expect(model.newOffenders[0]!.pct).toBe("new");
  });

  it("improvements: delta < -0.1/min, most-improved first (G7)", () => {
    expect(model.improvements.map((r) => r.loc)).toEqual(["imp"]);
    expect(model.improvements[0]!.delta).toBeCloseTo(-4, 6);
  });

  it("+∞% when P1's rate was 0 and P{last} > 0 (G4 edge)", () => {
    const zero = computeStats(resp(0, [loc("x", [], [])]), 100000);
    const grew = computeStats(resp(3, [loc("x", [200000, 200000, 200000], [1, 1, 1])]), 100000);
    expect(buildDiffModel([zero, grew], 2).cards.ratePctLabel).toBe("+∞%");
  });

  it("PRESERVED DEFECT (finding 4 / G3): throws when P1 is unloaded", () => {
    // P1 null while later periods loaded -> first.rate dereferences null,
    // exactly like the legacy renderDiffView TypeError. Not guarded.
    expect(() => buildDiffModel([null, p2, p2], 3)).toThrow();
  });
});

describe("shouldStopRefining (D3 termination)", () => {
  it("stops with no coverage, when fully folded, or when frozen", () => {
    expect(shouldStopRefining(null, -1)).toBe(true);
    expect(shouldStopRefining({ files_folded: 1, files_matched: 1 }, -1)).toBe(true);
    expect(shouldStopRefining({ files_folded: 2, files_matched: 5 }, 2)).toBe(true); // frozen
  });
  it("continues while more files can still be folded", () => {
    expect(shouldStopRefining({ files_folded: 2, files_matched: 5 }, 1)).toBe(false);
  });
});
