// Trace integrity checks, run against the demo trace (or an arbitrary trace
// file via the D9_TRACE_FILE env var — used by the stress workflow's
// offline-repro instructions; the workflow itself runs against the freshly
// regenerated public/demo-trace.bin in place).
//
// Migrated from test_trace_integrity.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale). The original's
// argv[2] trace-path override became the D9_TRACE_FILE env var per ADR-0004
// section 7. findTaskLifecycleInversions (formerly exported from this file
// for test_task_lifecycle.js) now lives in helpers/task_lifecycle_inversions.ts.
//
// Per T10 precedent, first-offender loops now collect ALL offenders and
// assert the collection is empty — strictly stronger and the failure output
// pinpoints every violation.

import { describe, it, expect, beforeAll } from "vitest";
import { createRequire } from "node:module";
import { existsSync, readFileSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { findTaskLifecycleInversions } from "./helpers/task_lifecycle_inversions";

const require = createRequire(import.meta.url);

interface TraceEvent {
  eventType: number;
  timestamp: number;
  workerId: number;
  taskId?: number;
  spawnLoc?: string | null;
  localQueue?: number;
  globalQueue?: number;
  cpuTime?: number;
  schedWait?: number;
  wokenTaskId?: number;
  targetWorker?: number;
}

interface ClockSyncAnchor {
  monotonicNs: number;
  realtimeNs: number;
}

interface CustomEvent {
  name: string;
  timestamp: number;
  fields: Record<string, unknown>;
  units: Record<string, string> | null;
}

interface ParsedTrace {
  events: TraceEvent[];
  version: number;
  truncated: boolean;
  taskSpawnLocs: Map<number, unknown>;
  taskSpawnTimes: Map<number, number>;
  taskTerminateTimes: Map<number, number>;
  clockSyncAnchors: ClockSyncAnchor[];
  clockOffsetNs: number | null;
  customEvents: CustomEvent[];
}

const { parseTrace, EVENT_TYPES } = require("../../trace_parser.js") as {
  parseTrace: (buf: Buffer) => Promise<ParsedTrace>;
  EVENT_TYPES: Record<string, number>;
};

const tracePath =
  process.env["D9_TRACE_FILE"] ??
  fileURLToPath(new URL("../../public/demo-trace.bin", import.meta.url));

let trace: ParsedTrace;

beforeAll(async () => {
  expect(existsSync(tracePath), `Trace file not found: ${tracePath}`).toBe(true);
  expect(statSync(tracePath).size, "Trace file is empty").toBeGreaterThan(0);
  trace = await parseTrace(readFileSync(tracePath));
});

// Worker ids from events that carry a real workerId. Note: default (lexical)
// sort, exactly as the original.
function getWorkerIds(): number[] {
  const eventsWithWorkerId = trace.events.filter(
    (e) =>
      e.eventType !== EVENT_TYPES["QueueSample"] &&
      e.eventType !== EVENT_TYPES["WakeEvent"],
  );
  return [...new Set(eventsWithWorkerId.map((e) => e.workerId))].sort();
}

describe("basic", () => {
  it("has events", () => {
    expect(trace.events.length, "No events found").toBeGreaterThan(0);
  });

  // One check per known event type: every type must appear in the trace.
  for (const [name, type] of Object.entries(EVENT_TYPES)) {
    it(`has ${name} events`, () => {
      const count = trace.events.filter((e) => e.eventType === type).length;
      expect(count, `No ${name} events found`).toBeGreaterThan(0);
    });
  }

  it("multiple workers", () => {
    const workerIds = getWorkerIds();
    expect(
      workerIds.length,
      `Only ${workerIds.length} worker(s)`,
    ).toBeGreaterThanOrEqual(2);
  });

  it("not truncated", () => {
    expect(trace.truncated, "Trace was truncated at event cap").toBe(false);
  });
});

describe("task tracking", () => {
  it("tasks spawned", () => {
    expect(trace.taskSpawnLocs.size, "No task spawned").toBeGreaterThan(0);
  });

  it("spawn locations resolved", () => {
    const pollStarts = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES["PollStart"],
    );
    const withSpawnLoc = pollStarts.filter((e) => !!e.spawnLoc);
    if (pollStarts.length > 0) {
      expect(withSpawnLoc.length, "No PollStart has spawnLoc").toBeGreaterThan(0);
    }
  });

  it("all polled tasks were spawned", () => {
    const pollStarts = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES["PollStart"],
    );
    const unspawnedTasks: number[] = [];
    for (const e of pollStarts) {
      if (e.taskId && !trace.taskSpawnLocs.has(e.taskId)) {
        unspawnedTasks.push(e.taskId);
      }
    }
    expect(
      unspawnedTasks.length,
      `task(s) polled but never spawned: ${unspawnedTasks.join(", ")}`,
    ).toBe(0);
  });

  it("task lifecycle consistency (spawn < terminate, sub-ms skew tolerated)", () => {
    // See findTaskLifecycleInversions: spawn/terminate timestamps are independent
    // per-worker CLOCK_MONOTONIC reads, so tiny cross-core skew can invert them
    // on virtualized runners. Tolerate that; fail only on seconds-scale
    // inversions that indicate real corruption.
    const { gross } = findTaskLifecycleInversions(
      trace.taskSpawnTimes,
      trace.taskTerminateTimes,
    );
    const worst = gross.reduce(
      (a, b) => (a === null || b.delta > a.delta ? b : a),
      null as { taskId: number; delta: number } | null,
    );
    expect(
      gross.length,
      `task(s) terminated before spawn beyond clock-skew tolerance` +
        (worst ? ` (worst: task ${worst.taskId}, ${worst.delta}ns before spawn)` : ""),
    ).toBe(0);
  });
});

describe("state machine", () => {
  it("PollStart/PollEnd pairing (no nested polls)", () => {
    const workerIds = getWorkerIds();
    const pollErrors: string[] = [];
    for (const wid of workerIds) {
      const wEvents = trace.events.filter(
        (e) =>
          e.workerId === wid &&
          [EVENT_TYPES["PollStart"], EVENT_TYPES["PollEnd"]].includes(e.eventType),
      );
      const bad = wEvents.find(
        (e, i) => i > 0 && e.eventType === wEvents[i - 1]!.eventType,
      );
      if (bad) {
        pollErrors.push(
          `worker ${wid}: duplicate ${
            bad.eventType === EVENT_TYPES["PollStart"] ? "PollStart" : "PollEnd"
          } at ts=${bad.timestamp}`,
        );
      }
    }
    expect(pollErrors, "duplicate PollStart/PollEnd").toEqual([]);
  });

  it("WorkerPark/WorkerUnpark pairing (no double park)", () => {
    const workerIds = getWorkerIds();
    const parkErrors: string[] = [];
    for (const wid of workerIds) {
      const wEvents = trace.events.filter(
        (e) =>
          e.workerId === wid &&
          [EVENT_TYPES["WorkerPark"], EVENT_TYPES["WorkerUnpark"]].includes(
            e.eventType,
          ),
      );
      const bad = wEvents.find(
        (e, i) => i > 0 && e.eventType === wEvents[i - 1]!.eventType,
      );
      if (bad) {
        parkErrors.push(
          `worker ${wid}: duplicate ${
            bad.eventType === EVENT_TYPES["WorkerPark"]
              ? "WorkerPark"
              : "WorkerUnpark"
          } at ts=${bad.timestamp}`,
        );
      }
    }
    expect(parkErrors, "duplicate WorkerPark/WorkerUnpark").toEqual([]);
  });
});

describe("field sanity", () => {
  it("timestamps are increasing per worker", () => {
    const workerIds = getWorkerIds();
    const tsErrors: string[] = [];
    for (const wid of workerIds) {
      const wEvents = trace.events.filter(
        (e) =>
          e.workerId === wid &&
          ![EVENT_TYPES["QueueSample"], EVENT_TYPES["WakeEvent"]].includes(
            e.eventType,
          ),
      );
      for (let i = 1; i < wEvents.length; i++) {
        if (wEvents[i]!.timestamp < wEvents[i - 1]!.timestamp) {
          tsErrors.push(
            `worker ${wid}: ts ${wEvents[i]!.timestamp} < ${
              wEvents[i - 1]!.timestamp
            } at index ${i}`,
          );
          break;
        }
      }
    }
    expect(tsErrors, "Timestamps are decreasing").toEqual([]);
  });

  it("queue depths non-negative", () => {
    const negQueue = trace.events.filter(
      (e) => (e.localQueue ?? 0) < 0 || (e.globalQueue ?? 0) < 0,
    );
    expect(
      negQueue.map(
        (e) =>
          `type=${e.eventType} localQueue=${e.localQueue} globalQueue=${e.globalQueue}`,
      ),
      "Negative queue depth",
    ).toEqual([]);
  });

  it("worker IDs bounded", () => {
    const workerIds = getWorkerIds();
    const maxWorkerId = Math.max(...workerIds);
    expect(
      maxWorkerId,
      `Unexpectedly large worker ID: ${maxWorkerId}`,
    ).toBeLessThanOrEqual(63);
  });

  it("cpuTime non-negative on Park/Unpark", () => {
    const parks = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES["WorkerPark"],
    );
    const unparks = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES["WorkerUnpark"],
    );
    const negCpuTime = parks
      .concat(unparks)
      .filter((e) => (e.cpuTime ?? 0) < 0);
    expect(
      negCpuTime.map((e) => `cpuTime ${e.cpuTime} at ts=${e.timestamp}`),
      "Negative cpuTime",
    ).toEqual([]);
  });

  it("schedWait non-negative on Unpark", () => {
    const unparks = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES["WorkerUnpark"],
    );
    const negSchedWait = unparks.filter((e) => (e.schedWait ?? 0) < 0);
    expect(
      negSchedWait.map((e) => `schedWait ${e.schedWait} at ts=${e.timestamp}`),
      "Negative schedWait",
    ).toEqual([]);
  });

  it("cpuTime populated", () => {
    const parks = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES["WorkerPark"],
    );
    const unparks = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES["WorkerUnpark"],
    );
    const nonZeroCpuTime = parks
      .concat(unparks)
      .filter((e) => (e.cpuTime ?? 0) > 0);
    expect(
      nonZeroCpuTime.length,
      "All cpuTime values are zero — instrumentation may be broken",
    ).toBeGreaterThan(0);
  });

  it("WakeEvent wokenTaskId references known tasks", () => {
    const wakeEvents = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES["WakeEvent"],
    );
    const unknownWoken = wakeEvents.filter(
      (e) => e.wokenTaskId && !trace.taskSpawnLocs.has(e.wokenTaskId),
    );
    expect(
      unknownWoken.length,
      `WakeEvent(s) reference unknown wokenTaskId (first: ${unknownWoken[0]?.wokenTaskId})`,
    ).toBe(0);
  });

  it("WakeEvent targetWorker within valid range", () => {
    const workerIds = getWorkerIds();
    const maxWorkerId = Math.max(...workerIds);
    const wakeEvents = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES["WakeEvent"],
    );
    const outOfRangeWorker = wakeEvents.filter(
      (e) =>
        e.targetWorker !== 255 && // UNKNOWN
        e.targetWorker !== 254 && // BLOCKING
        (e.targetWorker ?? 0) > maxWorkerId,
    );
    expect(
      outOfRangeWorker.map(
        (e) => `targetWorker ${e.targetWorker} exceeds max worker ID ${maxWorkerId}`,
      ),
      "WakeEvent targetWorker out of range",
    ).toEqual([]);
  });

  it("all PollStart events have a taskId", () => {
    const pollStarts = trace.events.filter(
      (e) => e.eventType === EVENT_TYPES["PollStart"],
    );
    const zeroTaskPolls = pollStarts.filter((e) => !e.taskId);
    expect(
      zeroTaskPolls.map((e) => `PollStart with zero taskId at ts=${e.timestamp}`),
      "PollStart with zero taskId",
    ).toEqual([]);
  });
});

describe("clock-sync", () => {
  it("clock offset reconstructs plausible wall clock", () => {
    expect(
      trace.clockSyncAnchors.length,
      "No clock-sync anchors (real or legacy-synthesized)",
    ).toBeGreaterThan(0);
    expect(trace.clockOffsetNs, "clockOffsetNs not derived from anchors").not.toBeNull();

    const a0 = trace.clockSyncAnchors[0]!;
    const reconstructedAnchorWall = a0.monotonicNs + trace.clockOffsetNs!;

    expect(
      a0.realtimeNs > a0.monotonicNs,
      `anchor values look wrong: realtimeNs=${a0.realtimeNs} monotonicNs=${a0.monotonicNs}`,
    ).toBe(true);

    // Offset must map anchor mono -> anchor wall.
    expect(
      Math.abs(reconstructedAnchorWall - a0.realtimeNs),
      "clockOffsetNs does not match first anchor",
    ).toBeLessThanOrEqual(1_000_000);

    // epoch-scale vs monotonic-scale sanity check
    const MIN_PLAUSIBLE_WALL_CLOCK_MS = 1_577_836_800_000; // 2020-01-01
    expect(
      reconstructedAnchorWall / 1e6,
      `reconstructed wall clock ${reconstructedAnchorWall} is implausibly old`,
    ).toBeGreaterThanOrEqual(MIN_PLAUSIBLE_WALL_CLOCK_MS);
  });
});

describe("metrique events", () => {
  function metriqueEvents(): CustomEvent[] {
    return trace.customEvents.filter((e) => e.name.startsWith("metrique:"));
  }

  it("demo app records metrique request metrics", () => {
    const events = metriqueEvents();
    expect(events.length, "No metrique:* events found").toBeGreaterThan(0);
    const names = new Set(events.map((e) => e.name));
    expect(names.has("metrique:RequestMetrics"), `names: ${[...names]}`).toBe(true);
  });

  it("metrique events carry context and payload fields", () => {
    for (const ev of metriqueEvents()) {
      const f = ev.fields;
      expect(f["worker_id"], `worker_id missing on ${ev.name}`).not.toBeNull();
      const end = Number(f["monotonic_ns_end"]);
      expect(end, `monotonic_ns_end missing on ${ev.name}`).toBeGreaterThan(0);
      expect(end, "end must be >= start timestamp").toBeGreaterThanOrEqual(ev.timestamp);
      expect(f["Operation"], `Operation missing on ${ev.name}`).toBeTruthy();
      expect(f["MetricName"], `MetricName missing on ${ev.name}`).toBeTruthy();
    }
  });

  it("metrique unit annotations surface as schema units", () => {
    const withLatency = metriqueEvents().filter((e) => e.fields["Latency"] != null);
    expect(withLatency.length, "No metrique events with a Latency field").toBeGreaterThan(0);
    for (const ev of withLatency) {
      expect(ev.units?.["Latency"], `Latency unit missing on ${ev.name}`).toBe("Milliseconds");
    }
  });
});
