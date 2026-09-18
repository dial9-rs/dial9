import { describe, expect, it } from "vitest";
import { WakeIndex, type WakeRecord } from "./wake-index.js";

/** ts, waker, woken, worker */
type Row = [number, number, number, number];

function index(rows: Row[]): WakeIndex {
  return WakeIndex.build(rows.length, (emit) => {
    for (const [ts, waker, woken, worker] of rows) emit(ts, waker, woken, worker);
  });
}

function drain(s: {
  length: number;
  timestampAt(i: number): number;
  wokenTaskIdAt(i: number): number;
  targetWorkerAt(i: number): number;
}): Row[] {
  const out: Row[] = [];
  for (let i = 0; i < s.length; i++) {
    out.push([s.timestampAt(i), NaN, s.wokenTaskIdAt(i), s.targetWorkerAt(i)]);
  }
  return out;
}

const ROWS: Row[] = [
  [10, 1, 7, 0],
  [20, 2, 8, 1],
  [30, 1, 7, 1],
  [40, 3, 9, 0],
  [50, 2, 7, 0],
];

describe("WakeIndex", () => {
  it("groups by woken task", () => {
    const w = index(ROWS);
    expect(drain(w.forTask(7)).map((r) => r[0])).toStrictEqual([10, 30, 50]);
    expect(drain(w.forTask(8)).map((r) => r[0])).toStrictEqual([20]);
    expect(drain(w.forTask(9)).map((r) => r[0])).toStrictEqual([40]);
  });

  it("groups by target worker independently of the task view", () => {
    const w = index(ROWS);
    expect(drain(w.forWorker(0)).map((r) => r[0])).toStrictEqual([10, 40, 50]);
    expect(drain(w.forWorker(1)).map((r) => r[0])).toStrictEqual([20, 30]);
  });

  it("sorts each group by timestamp, whatever order it was fed", () => {
    const w = index([
      [50, 2, 7, 0],
      [10, 1, 7, 0],
      [30, 1, 7, 1],
    ]);
    expect(drain(w.forTask(7)).map((r) => r[0])).toStrictEqual([10, 30, 50]);
    expect(drain(w.forWorker(0)).map((r) => r[0])).toStrictEqual([10, 50]);
  });

  it("keeps two live slices independent", () => {
    // A single shared cursor would make these interfere.
    const w = index(ROWS);
    const task = w.forTask(7);
    const worker = w.forWorker(1);
    expect(task.timestampAt(0)).toBe(10);
    expect(worker.timestampAt(0)).toBe(20);
    expect(task.timestampAt(1)).toBe(30);
    expect(worker.timestampAt(1)).toBe(30);
  });

  it("reads every field back off one row", () => {
    const w = index([[99, 4, 5, 2]]);
    const s = w.forTask(5);
    expect([
      s.timestampAt(0), s.wakerTaskIdAt(0), s.wokenTaskIdAt(0), s.targetWorkerAt(0),
    ]).toStrictEqual([99, 4, 5, 2]);
  });

  it("returns an empty slice for an unknown group", () => {
    const w = index(ROWS);
    expect(w.forTask(404).length).toBe(0);
    expect(w.forWorker(404).length).toBe(0);
  });

  it("materializes one task's wakes as records", () => {
    const w = index(ROWS);
    expect(w.materializeForTask(7)).toStrictEqual([
      { timestamp: 10, wakerTaskId: 1, wokenTaskId: 7, targetWorker: 0 },
      { timestamp: 30, wakerTaskId: 1, wokenTaskId: 7, targetWorker: 1 },
      { timestamp: 50, wakerTaskId: 2, wokenTaskId: 7, targetWorker: 0 },
    ]);
  });

  it("round-trips the frozen builder's records", () => {
    const byTask: Record<number, WakeRecord[]> = {
      7: [
        { timestamp: 10, wakerTaskId: 1, wokenTaskId: 7, targetWorker: 0 },
        { timestamp: 30, wakerTaskId: 1, wokenTaskId: 7, targetWorker: 1 },
      ],
      8: [{ timestamp: 20, wakerTaskId: 2, wokenTaskId: 8, targetWorker: 1 }],
    };
    const w = WakeIndex.fromRecords(byTask);
    expect(w.length).toBe(3);
    expect(w.materializeForTask(7)).toStrictEqual(byTask[7]);
    expect(drain(w.forWorker(1)).map((r) => r[0])).toStrictEqual([20, 30]);
  });

  it("handles no wakes at all", () => {
    const w = index([]);
    expect(w.length).toBe(0);
    expect(w.taskIds()).toStrictEqual([]);
    expect(w.forTask(1).length).toBe(0);
  });
});
