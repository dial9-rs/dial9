import { describe, expect, it } from "vitest";
import {
  MAX_RESIZABLE_ELEMENTS,
  allocColumn,
  resizeColumns,
} from "./resizable-column.js";

describe("resizable columns", () => {
  it("grows in place: same buffer, tracking view, contents preserved", () => {
    const col = allocColumn(Int32Array, 4);
    col.set([1, 2, 3, 4]);
    const before = col.buffer;

    expect(resizeColumns([col], 8)).toBe(true);
    expect(col.buffer).toBe(before);
    expect(col.length).toBe(8);
    expect([...col]).toEqual([1, 2, 3, 4, 0, 0, 0, 0]);
  });

  it("resizes a mixed-width set to the same element count", () => {
    const a = allocColumn(Uint8Array, 2);
    const b = allocColumn(Float64Array, 2);
    expect(resizeColumns([a, b], 6)).toBe(true);
    expect(a.length).toBe(6);
    expect(b.length).toBe(6);
  });

  it("declines past the ceiling, leaving every column untouched", () => {
    const a = allocColumn(Int32Array, 4);
    const b = allocColumn(Int32Array, 4);
    expect(resizeColumns([a, b], MAX_RESIZABLE_ELEMENTS + 1)).toBe(false);
    expect(a.length).toBe(4);
    expect(b.length).toBe(4);
  });

  // A store constructed at a capacity past the ceiling allocates fixed
  // buffers, so every later growth must take the copy path.
  it("allocates fixed past the ceiling and declines to resize it", () => {
    const col = allocColumn(Uint8Array, MAX_RESIZABLE_ELEMENTS + 1);
    expect(col.length).toBe(MAX_RESIZABLE_ELEMENTS + 1);
    expect(resizeColumns([col], MAX_RESIZABLE_ELEMENTS + 2)).toBe(false);
  });
});
