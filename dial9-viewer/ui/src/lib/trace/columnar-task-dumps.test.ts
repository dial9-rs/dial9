import { describe, expect, it } from "vitest";
import { ColumnarTaskDumps } from "./columnar-task-dumps.js";

function store(rows: [task: number, ts: number, chain: string[]][]): ColumnarTaskDumps {
  const s = new ColumnarTaskDumps();
  for (const [task, ts, chain] of rows) s.pushDump(task, ts, chain);
  return s;
}

describe("ColumnarTaskDumps", () => {
  it("groups by task and sorts each group by timestamp", () => {
    const s = store([
      [1, 30, ["0xa"]],
      [2, 10, ["0xb"]],
      [1, 10, ["0xc"]],
      [1, 20, ["0xd"]],
    ]);
    expect(s.get(1)!.map((d) => d.timestamp)).toStrictEqual([10, 20, 30]);
    expect(s.get(2)!.map((d) => d.timestamp)).toStrictEqual([10]);
  });

  it("round-trips each dump's callchain", () => {
    const s = store([
      [1, 1, ["0xa", "0xb", "0xc"]],
      [1, 2, []],
      [1, 3, ["0xb"]],
    ]);
    expect(s.get(1)!.map((d) => d.callchain)).toStrictEqual([
      ["0xa", "0xb", "0xc"],
      [],
      ["0xb"],
    ]);
  });

  it("interns repeated frames instead of holding a string each", () => {
    const s = store([
      [1, 1, ["0xa", "0xa", "0xa"]],
      [2, 1, ["0xa"]],
    ]);
    const a = s.get(1)!;
    // Same interned string reused across dumps and tasks.
    expect(a[0]!.callchain[0]).toBe(s.get(2)![0]!.callchain[0]);
  });

  it("reads an unknown task as undefined, and counts without materializing", () => {
    const s = store([[1, 1, ["0xa"]]]);
    expect(s.get(404)).toBeUndefined();
    expect(s.has(404)).toBe(false);
    expect(s.countForTask(1)).toBe(1);
    expect(s.countForTask(404)).toBe(0);
  });

  it("returns the same array for repeated reads of one task", () => {
    const s = store([[7, 1, ["0xa"]]]);
    expect(s.get(7)).toBe(s.get(7));
  });

  it("re-groups after a dump arrives post-read", () => {
    const s = store([[1, 10, ["0xa"]]]);
    expect(s.get(1)!.length).toBe(1);
    s.pushDump(1, 5, ["0xb"]);
    expect(s.get(1)!.map((d) => d.timestamp)).toStrictEqual([5, 10]);
  });

  it("grows past its initial capacity", () => {
    const rows: [number, number, string[]][] = [];
    for (let i = 0; i < 5000; i++) rows.push([i % 7, 5000 - i, ["0x" + (i % 50)]]);
    const s = store(rows);
    let total = 0;
    for (const k of s.keys()) total += s.countForTask(k);
    expect(total).toBe(5000);
    const one = s.get(0)!;
    expect(one.length).toBeGreaterThan(700);
    for (let i = 1; i < one.length; i++) {
      expect(one[i]!.timestamp).toBeGreaterThan(one[i - 1]!.timestamp);
    }
  });

  it("handles no dumps", () => {
    const s = new ColumnarTaskDumps();
    expect(s.size).toBe(0);
    expect(s.get(1)).toBeUndefined();
  });
});

// Parity against the Map the frozen parser builds without a sink: same tasks,
// same dumps, same order, same frames.
describe("parser sink parity", () => {
  it("matches the fat Map on the demo trace", async () => {
    const { readFileSync } = await import("node:fs");
    await import("./core-globals.js");
    const { parseTrace } = await import("../../../trace_parser.js");
    const raw = readFileSync("public/demo-trace.bin");
    const bytes = (): ArrayBuffer =>
      raw.buffer.slice(raw.byteOffset, raw.byteOffset + raw.byteLength) as ArrayBuffer;

    const fat = await parseTrace(bytes(), {} as never);
    const sink = new ColumnarTaskDumps();
    const col = await parseTrace(bytes(), { taskDumpSink: sink } as never);

    const fatMap = fat.taskDumps as unknown as Map<number, TaskDumpRec[]>;
    expect(fatMap.size).toBeGreaterThan(0);
    expect(col.taskDumps).toBe(sink);
    expect([...sink.keys()].sort((a, b) => a - b)).toStrictEqual(
      [...fatMap.keys()].sort((a, b) => a - b),
    );
    for (const [taskId, want] of fatMap) {
      expect(sink.countForTask(taskId), `task ${taskId} count`).toBe(want.length);
      expect(sink.get(taskId), `task ${taskId} dumps`).toStrictEqual(want);
    }
  }, 300000);
});

interface TaskDumpRec {
  timestamp: number;
  callchain: string[];
}
