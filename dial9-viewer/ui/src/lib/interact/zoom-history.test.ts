// The bounded zoom-history stack behind the unified `z` key: record/undo
// semantics, the no-baseline first record, equality dedup, depth bound, clear.

import { describe, it, expect } from "vitest";
import { createZoomHistory } from "./zoom-history.js";

describe("createZoomHistory", () => {
  it("undo with no history returns null", () => {
    const h = createZoomHistory<number>();
    expect(h.undo()).toBeNull();
    expect(h.depth()).toBe(0);
  });

  it("the first record establishes the baseline and pushes nothing", () => {
    const h = createZoomHistory<number>();
    h.record(1);
    expect(h.depth()).toBe(0);
    expect(h.undo()).toBeNull();
  });

  it("record tracks current, undo steps back through past states", () => {
    const h = createZoomHistory<number>();
    h.record(1);
    h.record(2);
    h.record(3);
    expect(h.depth()).toBe(2);
    expect(h.undo()).toBe(2);
    expect(h.undo()).toBe(1);
    expect(h.undo()).toBeNull();
  });

  it("recording after an undo branches from the restored state", () => {
    const h = createZoomHistory<number>();
    h.record(1);
    h.record(2);
    expect(h.undo()).toBe(1);
    h.record(3); // 1 -> 3; the undone 2 is gone
    expect(h.undo()).toBe(1);
    expect(h.undo()).toBeNull();
  });

  it("equal records are ignored (no no-op undo steps)", () => {
    const h = createZoomHistory<number>();
    h.record(1);
    h.record(1);
    h.record(1);
    expect(h.depth()).toBe(0);
  });

  it("custom equality dedups structurally equal states", () => {
    const h = createZoomHistory<{ w: string[] }>({
      equals: (a, b) => a.w.join("\t") === b.w.join("\t"),
    });
    h.record({ w: ["a"] });
    h.record({ w: ["a"] }); // structurally equal - ignored
    expect(h.depth()).toBe(0);
    h.record({ w: ["a", "b"] });
    expect(h.depth()).toBe(1);
    expect(h.undo()).toEqual({ w: ["a"] });
  });

  it("the stack is bounded: oldest entries drop beyond the limit", () => {
    const h = createZoomHistory<number>({ limit: 3 });
    for (let i = 0; i <= 10; i++) h.record(i);
    expect(h.depth()).toBe(3);
    expect(h.undo()).toBe(9);
    expect(h.undo()).toBe(8);
    expect(h.undo()).toBe(7);
    expect(h.undo()).toBeNull();
  });

  it("clear drops history AND the baseline", () => {
    const h = createZoomHistory<number>();
    h.record(1);
    h.record(2);
    h.clear();
    expect(h.depth()).toBe(0);
    expect(h.undo()).toBeNull();
    // Post-clear, the next record is a fresh baseline.
    h.record(5);
    expect(h.depth()).toBe(0);
  });
});
