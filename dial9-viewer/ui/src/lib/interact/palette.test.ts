// Palette filtering + the selection-stepping policy. The policy is pure and
// tested here; the component's DOM shell is exercised by the parity axe scan.

import { describe, it, expect } from "vitest";
import { filterPaletteItems, stepClamped, stepWrapped } from "./palette.js";
import type { PaletteItem } from "./palette.js";

// The mock's dataset shape (kinds + labels).
const ITEMS: PaletteItem[] = [
  { kind: "task", label: "0x3a axum_traced.rs:243:33", detail: "+0.31s" },
  { kind: "task", label: "0x51 hyper::proto conn", detail: "+0.02s" },
  { kind: "poi", label: "sched delay 84ms W1", detail: "+1.72s" },
  { kind: "poi", label: "long poll 61ms W0", detail: "+2.03s" },
  { kind: "span", label: "handle_request", detail: "+1.10s" },
  { kind: "span", label: "db_query users", detail: "+1.31s" },
];

describe("filterPaletteItems (mock matching rule)", () => {
  it("empty / whitespace query keeps everything", () => {
    expect(filterPaletteItems(ITEMS, "")).toEqual(ITEMS);
    expect(filterPaletteItems(ITEMS, "   ")).toEqual(ITEMS);
  });

  it("case-insensitive substring on the label", () => {
    const hits = filterPaletteItems(ITEMS, "HYPER");
    expect(hits).toHaveLength(1);
    expect(hits[0]?.label).toContain("hyper");
  });

  it("matches across the '<kind> <label>' concatenation", () => {
    // Kind-qualified narrowing: "poi sched" hits only the sched-delay POI.
    const hits = filterPaletteItems(ITEMS, "poi sched");
    expect(hits).toHaveLength(1);
    expect(hits[0]?.kind).toBe("poi");
  });

  it("kind-only query selects the whole kind", () => {
    expect(filterPaletteItems(ITEMS, "span")).toHaveLength(2);
    expect(filterPaletteItems(ITEMS, "task")).toHaveLength(2);
  });

  it("no matches -> empty list (renders 'no matches', never throws)", () => {
    expect(filterPaletteItems(ITEMS, "zebra")).toEqual([]);
  });

  it("special characters are literal, not regex", () => {
    expect(filterPaletteItems(ITEMS, "rs:243:33")).toHaveLength(1);
    expect(filterPaletteItems(ITEMS, ".*")).toEqual([]);
  });
});

describe("stepClamped (ArrowUp/ArrowDown, mock semantics)", () => {
  it("clamps at both ends instead of wrapping", () => {
    expect(stepClamped(0, -1, 3)).toBe(0);
    expect(stepClamped(2, 1, 3)).toBe(2);
    expect(stepClamped(1, 1, 3)).toBe(2);
    expect(stepClamped(1, -1, 3)).toBe(0);
  });

  it("empty list pins the index at 0", () => {
    expect(stepClamped(5, 1, 0)).toBe(0);
  });
});

describe("stepWrapped (Enter-cycling)", () => {
  it("Enter advances and wraps forward", () => {
    expect(stepWrapped(0, 1, 3)).toBe(1);
    expect(stepWrapped(2, 1, 3)).toBe(0);
  });

  it("Shift+Enter steps back and wraps to the end", () => {
    expect(stepWrapped(0, -1, 3)).toBe(2);
    expect(stepWrapped(1, -1, 3)).toBe(0);
  });

  it("single result cycles onto itself", () => {
    expect(stepWrapped(0, 1, 1)).toBe(0);
    expect(stepWrapped(0, -1, 1)).toBe(0);
  });

  it("empty list pins the index at 0", () => {
    expect(stepWrapped(0, 1, 0)).toBe(0);
  });
});
