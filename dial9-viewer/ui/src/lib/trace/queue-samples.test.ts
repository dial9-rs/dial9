import { describe, expect, it } from "vitest";
import { QueueSampleIndex } from "./queue-samples.js";

function drain(s: { length: number; tAt(i: number): number; localAt(i: number): number }) {
  return Array.from({ length: s.length }, (_, i) => [s.tAt(i), s.localAt(i)]);
}

describe("QueueSampleIndex", () => {
  it("keeps each worker's samples in push order", () => {
    const q = new QueueSampleIndex();
    q.push(0, 10, 3);
    q.push(1, 20, 7);
    q.push(0, 30, 5);
    expect(drain(q.forWorker(0))).toStrictEqual([[10, 3], [30, 5]]);
    expect(drain(q.forWorker(1))).toStrictEqual([[20, 7]]);
  });

  it("grows past its initial capacity without reordering", () => {
    const q = new QueueSampleIndex();
    for (let i = 0; i < 1000; i++) q.push(0, i, i % 256);
    const s = q.forWorker(0);
    expect(s.length).toBe(1000);
    expect([s.tAt(0), s.tAt(999)]).toStrictEqual([0, 999]);
    expect(s.localAt(999)).toBe(999 % 256);
  });

  it("holds the full u8 range of queue depth", () => {
    const q = new QueueSampleIndex();
    q.push(0, 1, 255);
    expect(q.forWorker(0).localAt(0)).toBe(255);
  });

  it("reads an unknown worker as empty, and a registered one as present", () => {
    const q = new QueueSampleIndex();
    q.ensure(4);
    expect(q.forWorker(4).length).toBe(0);
    expect(q.forWorker(99).length).toBe(0);
    expect(q.workerIds()).toStrictEqual([4]);
  });

  it("round-trips the frozen builder's map", () => {
    const map = { 0: [{ t: 10, local: 3 }, { t: 30, local: 5 }], 1: [{ t: 20, local: 7 }] };
    const q = QueueSampleIndex.fromRecordMap(map);
    expect(q.toRecordMap()).toStrictEqual(map);
  });
});
