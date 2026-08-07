// Tests for the Tasks-tab model. The aggregation is exercised against the demo
// trace (its own reduction re-derived independently for a parity check), and
// the pure sort + windowing are unit-tested on hand-built rows.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "../../lib/trace/index.js";
import { deriveWorkerIds } from "../../lib/trace/derived.js";
import { buildWorkerSpans } from "../../lib/trace/index.js";
import type { ParsedTrace } from "../../types/trace.js";
import type { PoiSlice } from "../../types/state.js";
import {
  deriveTaskViewModel,
  sortTasks,
  taskIndexFor,
  taskIdLabel,
  durationCell,
  type TaskIndexRow,
  type TaskSortKey,
} from "./tasks-model.js";

const POI: PoiSlice = {
  filter: "sched",
  sortKey: "duration",
  sortDir: "desc",
  index: -1,
  railTab: "tasks",
  taskSort: "total",
  taskSortDir: "desc",
  taskIndex: -1,
};

let trace: ParsedTrace;

beforeAll(async () => {
  const b = readFileSync(fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)));
  const raw = b[0] === 0x1f && b[1] === 0x8b ? new Uint8Array(gunzipSync(b)) : new Uint8Array(b);
  trace = await parseTraceBuffer(raw);
});

/** Re-derive per-task poll aggregates straight from the fat spans, independent
 *  of the model, so the index can be checked against ground truth. */
function groundTruth(t: ParsedTrace): Map<number, { polls: number; total: number; longest: number }> {
  const ids = deriveWorkerIds(t);
  const r = buildWorkerSpans(t.events, ids, t.maxTs ?? 0, t.blockInPlaceGaps);
  const out = new Map<number, { polls: number; total: number; longest: number }>();
  for (const w of ids) {
    for (const p of r.workerSpans[w]?.polls ?? []) {
      const dur = p.end - p.start;
      const a = out.get(p.taskId) ?? { polls: 0, total: 0, longest: 0 };
      a.polls++;
      a.total += dur;
      if (dur > a.longest) a.longest = dur;
      out.set(p.taskId, a);
    }
  }
  return out;
}

describe("taskIndexFor", () => {
  it("aggregates poll count / total / longest to match a direct reduction", () => {
    const rows = taskIndexFor(trace).rows;
    const truth = groundTruth(trace);
    expect(truth.size).toBeGreaterThan(0);
    const byId = new Map(rows.map((r) => [r.taskId, r]));
    for (const [taskId, a] of truth) {
      const row = byId.get(taskId);
      expect(row, `task ${taskId} present`).toBeDefined();
      expect(row!.pollCount).toBe(a.polls);
      expect(row!.totalPollNs).toBe(a.total);
      expect(row!.longestPollNs).toBe(a.longest);
    }
  });

  it("includes every polled task AND every spawned task", () => {
    const rows = taskIndexFor(trace).rows;
    const ids = new Set(rows.map((r) => r.taskId));
    for (const id of trace.taskSpawnTimes.keys()) {
      expect(ids.has(id), `spawned task ${id} listed`).toBe(true);
    }
    for (const id of groundTruth(trace).keys()) {
      expect(ids.has(id), `polled task ${id} listed`).toBe(true);
    }
  });

  it("never produces NaN in a numeric field", () => {
    for (const r of taskIndexFor(trace).rows) {
      expect(Number.isNaN(r.totalPollNs)).toBe(false);
      expect(Number.isNaN(r.longestPollNs)).toBe(false);
      expect(r.lifetimeNs === null || !Number.isNaN(r.lifetimeNs)).toBe(true);
    }
  });

  it("memoizes on trace identity", () => {
    expect(taskIndexFor(trace)).toBe(taskIndexFor(trace));
  });

  it("firstPollWorker is one of the workers the task ran on (reveal target)", () => {
    const rows = taskIndexFor(trace).rows.filter((r) => r.pollCount > 0);
    expect(rows.length).toBeGreaterThan(0);
    for (const r of rows) {
      expect(r.firstPollWorker).toBeGreaterThanOrEqual(0);
      expect(r.firstPollStart).toBeGreaterThanOrEqual(0);
    }
  });
});

describe("deriveTaskViewModel", () => {
  it("is empty with no trace", () => {
    const vm = deriveTaskViewModel(null, POI);
    expect(vm.total).toBe(0);
    expect(vm.rows).toEqual([]);
  });

  // The empty state must tell "this trace has no tasks" apart from "coverage
  // is partial", so it reads coverage rather than whether task data exists.
  // A blank lifetime column means "not recorded", not "instant tasks", so the
  // model has to carry that apart from coverage.
  it("carries lifetime availability separately from coverage", () => {
    const noLifetimes = {
      ...trace,
      hasFullTaskCoverage: true,
      hasTaskLifetimes: false,
    } as unknown as typeof trace;
    const vm = deriveTaskViewModel(noLifetimes, POI);
    expect(vm.hasFullTaskCoverage).toBe(true);
    expect(vm.hasTaskLifetimes).toBe(false);
  });

  it("carries task coverage, not task-data existence", () => {
    expect(deriveTaskViewModel(trace, POI).hasFullTaskCoverage).toBe(
      trace.hasFullTaskCoverage,
    );

    const partial = {
      ...trace,
      hasFullTaskCoverage: false,
    } as unknown as typeof trace;
    expect(deriveTaskViewModel(partial, POI).hasFullTaskCoverage).toBe(false);
  });

  it("reports the full task count and formats at most a window of rows", () => {
    const vm = deriveTaskViewModel(trace, POI);
    expect(vm.total).toBe(taskIndexFor(trace).rows.length);
    expect(vm.rows.length).toBeLessThanOrEqual(vm.total);
    expect(vm.rows.length).toBeLessThanOrEqual(500);
    expect(vm.sorted.length).toBe(vm.total);
  });

  it("sorts by total poll time descending by default", () => {
    const vm = deriveTaskViewModel(trace, POI);
    for (let i = 1; i < vm.sorted.length; i++) {
      expect(vm.sorted[i - 1]!.totalPollNs).toBeGreaterThanOrEqual(vm.sorted[i]!.totalPollNs);
    }
  });
});

// ── Pure sort + formatting ─────────────────────────────────────────────────

function row(over: Partial<TaskIndexRow>): TaskIndexRow {
  return {
    taskId: 1,
    spawnLoc: null,
    spawnTs: null,
    terminateTs: null,
    lifetimeNs: null,
    instrumented: true,
    pollCount: 0,
    totalPollNs: 0,
    longestPollNs: 0,
    firstPollStart: -1,
    firstPollWorker: -1,
    workerCount: 0,
    ...over,
  };
}

describe("sortTasks", () => {
  const rows: TaskIndexRow[] = [
    row({ taskId: 1, pollCount: 3, totalPollNs: 30, spawnLoc: "b.rs:2" }),
    row({ taskId: 2, pollCount: 1, totalPollNs: 50, spawnLoc: "a.rs:1" }),
    row({ taskId: 3, pollCount: 3, totalPollNs: 10, spawnLoc: null }),
  ];

  it("orders by the chosen numeric column and direction", () => {
    expect(sortTasks(rows, "total", "desc").map((r) => r.taskId)).toEqual([2, 1, 3]);
    expect(sortTasks(rows, "total", "asc").map((r) => r.taskId)).toEqual([3, 1, 2]);
  });

  it("breaks ties on task id for a stable order", () => {
    // tasks 1 and 3 both have pollCount 3; ascending id wins the tie.
    expect(sortTasks(rows, "polls", "desc").map((r) => r.taskId)).toEqual([1, 3, 2]);
  });

  it("sorts a null spawn location as empty string, never throwing", () => {
    expect(sortTasks(rows, "loc", "asc").map((r) => r.taskId)).toEqual([3, 2, 1]);
  });

  it("clusters unknown lifetimes at the low end", () => {
    const withLife = [
      row({ taskId: 1, lifetimeNs: 100 }),
      row({ taskId: 2, lifetimeNs: null }),
      row({ taskId: 3, lifetimeNs: 50 }),
    ];
    expect(sortTasks(withLife, "lifetime", "asc").map((r) => r.taskId)).toEqual([2, 3, 1]);
  });
});

describe("formatting", () => {
  it("labels task ids in hex", () => {
    expect(taskIdLabel(255)).toBe("0xff");
  });
  it("renders a placeholder for absent / negative durations", () => {
    expect(durationCell(null)).toBe("-");
    expect(durationCell(-1)).toBe("-");
    expect(durationCell(0)).not.toBe("-");
  });
});
