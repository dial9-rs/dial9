// Keyboard region/zoom selection machine: every transition including the
// cancel paths.

import { describe, it, expect } from "vitest";
import {
  createKbSelectionMachine,
  MIN_SELECTION_NS,
  type KbSelectionCommand,
} from "./kb-selection.js";

function pick<T extends KbSelectionCommand["type"]>(
  cmds: readonly KbSelectionCommand[],
  type: T,
): Extract<KbSelectionCommand, { type: T }> | undefined {
  return cmds.find((c) => c.type === type) as Extract<KbSelectionCommand, { type: T }> | undefined;
}

describe("createKbSelectionMachine - start", () => {
  it("region start seeds the cursor at the anchor + announces", () => {
    const m = createKbSelectionMachine();
    const cmds = m.start("region-select", 5_000);
    expect(m.active()).toBe(true);
    expect(m.mode()).toBe("region-select");
    expect(pick(cmds, "set-kb")?.selection).toEqual({
      kind: "region-select",
      startNs: 5_000,
      cursorNs: 5_000,
    });
    expect(pick(cmds, "announce")?.message).toMatch(/Region selection started/);
  });

  it("zoom start announces the zoom variant", () => {
    const m = createKbSelectionMachine();
    expect(pick(m.start("zoom-select", 1), "announce")?.message).toMatch(/Zoom selection started/);
  });

  it("pressing the SAME modifier again confirms (toggle)", () => {
    const m = createKbSelectionMachine();
    m.start("region-select", 1_000);
    m.extend(1, 5_000, 0, 100_000); // cursor -> 6_000
    const cmds = m.start("region-select", 999); // same mode => confirm
    expect(m.active()).toBe(false);
    expect(pick(cmds, "commit-region")).toEqual({
      type: "commit-region",
      kind: "region-select",
      startNs: 1_000,
      endNs: 6_000,
    });
  });

  it("pressing a DIFFERENT modifier while active is a no-op", () => {
    const m = createKbSelectionMachine();
    m.start("region-select", 1_000);
    expect(m.start("zoom-select", 2_000)).toEqual([]);
    expect(m.active()).toBe(true);
    expect(m.mode()).toBe("region-select");
  });
});

describe("createKbSelectionMachine - extend", () => {
  it("moves the cursor by the step and clamps to [minTs,maxTs]", () => {
    const m = createKbSelectionMachine();
    m.start("region-select", 5_000);
    const right = pick(m.extend(1, 2_000, 0, 6_000), "set-kb")?.selection;
    expect(right?.cursorNs).toBe(6_000); // 5000+2000=7000 clamped to maxTs 6000
    const left = pick(m.extend(-1, 10_000, 3_000, 6_000), "set-kb")?.selection;
    expect(left?.cursorNs).toBe(3_000); // 6000-10000 clamped to minTs 3000
  });

  it("extend while idle is a no-op", () => {
    const m = createKbSelectionMachine();
    expect(m.extend(1, 100, 0, 1000)).toEqual([]);
  });
});

describe("createKbSelectionMachine - confirm", () => {
  it("commits [min,max] and clears when >= 100ns", () => {
    const m = createKbSelectionMachine();
    m.start("zoom-select", 10_000);
    m.extend(-1, 4_000, 0, 100_000); // cursor -> 6_000
    const cmds = m.confirm();
    expect(pick(cmds, "set-kb")?.selection).toBeNull();
    expect(pick(cmds, "commit-region")).toMatchObject({
      kind: "zoom-select",
      startNs: 6_000,
      endNs: 10_000,
    });
    expect(pick(cmds, "announce")?.message).toBe("Zoomed to selected region");
    expect(m.active()).toBe(false);
  });

  it("selection under 100ns clears WITHOUT committing", () => {
    const m = createKbSelectionMachine();
    m.start("region-select", 1_000);
    m.extend(1, MIN_SELECTION_NS - 1, 0, 100_000); // 99ns span
    const cmds = m.confirm();
    expect(pick(cmds, "set-kb")?.selection).toBeNull();
    expect(pick(cmds, "commit-region")).toBeUndefined();
    expect(m.active()).toBe(false);
  });

  it("confirm while idle is a no-op", () => {
    expect(createKbSelectionMachine().confirm()).toEqual([]);
  });
});

describe("createKbSelectionMachine - cancel / clear", () => {
  it("cancel clears + announces", () => {
    const m = createKbSelectionMachine();
    m.start("region-select", 1_000);
    const cmds = m.cancel();
    expect(pick(cmds, "set-kb")?.selection).toBeNull();
    expect(pick(cmds, "announce")?.message).toBe("Selection cancelled");
    expect(m.active()).toBe(false);
  });

  it("clear clears SILENTLY (no announce) - the mousedown pre-empt path", () => {
    const m = createKbSelectionMachine();
    m.start("zoom-select", 1_000);
    const cmds = m.clear();
    expect(pick(cmds, "set-kb")?.selection).toBeNull();
    expect(pick(cmds, "announce")).toBeUndefined();
    expect(m.active()).toBe(false);
  });

  it("cancel/clear while idle are no-ops", () => {
    const m = createKbSelectionMachine();
    expect(m.cancel()).toEqual([]);
    expect(m.clear()).toEqual([]);
  });
});
