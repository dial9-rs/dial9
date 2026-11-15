// Tests for the dial9-red-flags skill's scanner, red_flag_scan.js.

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

const scanJsPath = fileURLToPath(
  new URL("../../../skills/dial9-red-flags/scripts/red_flag_scan.js", import.meta.url),
);
const demoPath = fileURLToPath(new URL("../../public/demo-trace.bin", import.meta.url));

interface Finding { severity: string; check: string; message: string }
interface Poll { start: number; end: number; taskId: number }
interface CustomEvent {
  name: string;
  timestamp: number;
  fields: Record<string, unknown>;
  singleEventSpan?: Record<string, unknown>;
}

interface Active { start: number; end: number; ratio: number }
interface WorkerEvent { eventType: number; workerId: number; timestamp: number; cpuTime: number }

const { spanChecks, cpuContentionChecks } = require(scanJsPath) as {
  spanChecks: (
    trace: { customEvents: CustomEvent[] },
    workerIds: number[],
    workerSpans: Record<number, { polls: Poll[] }>,
  ) => Finding[];
  cpuContentionChecks: (
    trace: { events: WorkerEvent[] },
    workerIds: number[],
    workerSpans: Record<number, { actives: Active[] }>,
  ) => Finding[];
};
const { EVENT_TYPES } = require("../../trace_parser.js") as {
  EVENT_TYPES: { WorkerPark: number; WorkerUnpark: number };
};

const TASK = 7;

// Span events as current producers write them: the owning task id, no worker.
function enter(spanId: number, name: string, timestamp: number): CustomEvent {
  return {
    name: `SpanEnter:app::${name}:src/main.rs:1`,
    timestamp,
    fields: { span_id: spanId, span_name: name, "dial9.tokio.task_id": TASK },
  };
}
function exit(spanId: number, name: string, timestamp: number): CustomEvent {
  return { name: `SpanExit:app::${name}:src/main.rs:1`, timestamp, fields: { span_id: spanId } };
}
function span(spanId: number, name: string, start: number, end: number): CustomEvent[] {
  return [enter(spanId, name, start), exit(spanId, name, end)];
}

function checks(customEvents: CustomEvent[], polls: Poll[]): Finding[] {
  return spanChecks({ customEvents }, [0], { 0: { polls } });
}

describe("red_flag_scan span checks", () => {
  it("scans a trace with custom events to the end", () => {
    const run = spawnSync(process.execPath, [scanJsPath, demoPath], { encoding: "utf8" });
    expect(run.status, run.stderr).toBe(0);
    expect(run.stdout).toContain("[span-duration-outlier]");
  });

  it("flags a poll that runs many spans", () => {
    const events = Array.from({ length: 25 }, (_, i) =>
      span(i + 1, "step", 1000 + i * 30, 1010 + i * 30),
    ).flat();
    const findings = checks(events, [{ start: 1000, end: 2000, taskId: TASK }]);
    expect(findings.filter((f) => f.check === "many-spans-per-poll")).toEqual([
      { severity: "warning", check: "many-spans-per-poll", message: "Worker 0: poll with 25 spans (0.00ms)" },
    ]);
  });

  it("does not count single-event spans as enter/exit pairs", () => {
    const events = Array.from({ length: 25 }, (_, i): CustomEvent => ({
      name: "RequestMetrics",
      timestamp: 1000 + i * 30,
      fields: {},
      singleEventSpan: {
        name: "RequestMetrics", spanType: "metrique", fields: {}, units: {},
        start: 1000 + i * 30, end: 1010 + i * 30, taskId: TASK,
      },
    }));
    const findings = checks(events, [{ start: 1000, end: 2000, taskId: TASK }]);
    expect(findings.filter((f) => f.check === "many-spans-per-poll")).toEqual([]);
  });

  it("flags span duration outliers", () => {
    const events = Array.from({ length: 12 }, (_, i) =>
      span(i + 1, "query", i * 2000, i * 2000 + (i === 11 ? 1500 : 100)),
    ).flat();
    const findings = checks(events, [{ start: 0, end: 30_000, taskId: TASK }]);
    expect(findings.map((f) => f.check)).toEqual(["span-duration-outlier"]);
    expect(findings[0]?.message).toMatch(/^query: 1 spans >10x P50/);
  });

  it("flags spans entered but never exited", () => {
    const events = [...span(1, "done", 1000, 1100), enter(2, "pending", 1200)];
    const findings = checks(events, [{ start: 1000, end: 2000, taskId: TASK }]);
    expect(findings).toEqual([
      { severity: "info", check: "unmatched-spans", message: "1 spans with enter but no exit: pending(1)" },
    ]);
  });
});

describe("red_flag_scan cpu-contention check", () => {
  // Twelve 2ms active periods at a CPU ratio of 0.2, bounded by unpark/park
  // events carrying `cpuTime`.
  function worker(workerId: number, cpuTime: (i: number) => number) {
    const actives: Active[] = [];
    const events: WorkerEvent[] = [];
    for (let i = 0; i < 12; i++) {
      const start = i * 3e6;
      actives.push({ start, end: start + 2e6, ratio: 0.2 });
      events.push(
        { eventType: EVENT_TYPES.WorkerUnpark, workerId, timestamp: start, cpuTime: cpuTime(2 * i) },
        { eventType: EVENT_TYPES.WorkerPark, workerId, timestamp: start + 2e6, cpuTime: cpuTime(2 * i + 1) },
      );
    }
    return { events, actives };
  }
  const timed = (n: number) => n * 4e5;
  const untimed = () => 0;

  it("flags a worker that got little CPU while active", () => {
    const w0 = worker(0, timed);
    const findings = cpuContentionChecks({ events: w0.events }, [0], { 0: w0 });
    expect(findings.map((f) => [f.severity, f.check])).toEqual([["warning", "cpu-contention"]]);
  });

  it("does not check a worker whose events carry no CPU time, and says so", () => {
    const w0 = worker(0, timed);
    const w1 = worker(1, untimed);
    const findings = cpuContentionChecks({ events: [...w0.events, ...w1.events] }, [0, 1], { 0: w0, 1: w1 });
    expect(findings.map((f) => [f.severity, f.message.slice(0, 16)])).toEqual([
      ["warning", "Worker 0: 12/12 "],
      ["info", "Not checked for "],
    ]);
    expect(findings[1]?.message).toMatch(/^Not checked for 1 worker\(s\)/);
  });
});
