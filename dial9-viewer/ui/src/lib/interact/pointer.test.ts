// T23 pointer-machine transitions (DoD: every transition incl. cancel paths).
// Pure machine, no DOM: drive it with abstract down/move/up/cancel and assert
// the emitted commands + phase. Thresholds preserved (3px drag intent); the
// legacy pan/region/zoom/click discrimination (viewer.html:5145-5346).

import { describe, it, expect } from "vitest";
import {
  createPointerMachine,
  panWindowByPixels,
  DRAG_INTENT_PX,
  type PointerCommand,
  type PointerDownInput,
} from "./pointer.js";

const BASE_DOWN: PointerDownInput = {
  clientX: 100,
  shiftKey: false,
  altKey: false,
  nsAt: 5_000,
  viewStart: 0,
  viewEnd: 10_000,
  minTs: 0,
  maxTs: 10_000,
  hasTrace: true,
};

function down(over: Partial<PointerDownInput> = {}): PointerDownInput {
  return { ...BASE_DOWN, ...over };
}

/** First command of a given type, or undefined. */
function cmd<T extends PointerCommand["type"]>(
  cmds: readonly PointerCommand[],
  type: T,
): Extract<PointerCommand, { type: T }> | undefined {
  return cmds.find((c) => c.type === type) as Extract<PointerCommand, { type: T }> | undefined;
}

describe("createPointerMachine - down transitions", () => {
  it("no trace: no gesture, stays idle", () => {
    const m = createPointerMachine();
    expect(m.down(down({ hasTrace: false }))).toEqual([]);
    expect(m.phase()).toBe("idle");
  });

  it("plain press starts a pan (grabbing cursor + pan drag snapshot)", () => {
    const m = createPointerMachine();
    const cmds = m.down(down());
    expect(m.phase()).toBe("pan");
    expect(cmd(cmds, "cursor")?.cursor).toBe("grabbing");
    const drag = cmd(cmds, "set-drag")?.drag;
    expect(drag).toMatchObject({ kind: "pan", startX: 100, startNs: 5_000, curNs: 5_000, moved: false });
  });

  it("Shift press starts a region select (crosshair cursor)", () => {
    const m = createPointerMachine();
    const cmds = m.down(down({ shiftKey: true }));
    expect(m.phase()).toBe("select");
    expect(cmd(cmds, "cursor")?.cursor).toBe("crosshair");
    expect(cmd(cmds, "set-drag")?.drag.kind).toBe("region-select");
  });

  it("Alt press starts a zoom select", () => {
    const m = createPointerMachine();
    const cmds = m.down(down({ altKey: true }));
    expect(cmd(cmds, "set-drag")?.drag.kind).toBe("zoom-select");
  });

  it("Shift wins over Alt (Shift+Alt = region select)", () => {
    const m = createPointerMachine();
    const cmds = m.down(down({ shiftKey: true, altKey: true }));
    expect(cmd(cmds, "set-drag")?.drag.kind).toBe("region-select");
  });
});

describe("createPointerMachine - pan move/up + click discrimination", () => {
  it("small move (<= 3px) is NOT a drag; the click still fires", () => {
    const m = createPointerMachine();
    m.down(down());
    m.move({ clientX: 100 + DRAG_INTENT_PX, nsAtClamped: 5_100, drawW: 1000 });
    expect(m.moved()).toBe(false); // exactly 3px does not exceed the threshold
    m.up();
    expect(m.moved()).toBe(false); // click handler will proceed
  });

  it("move past 3px pans and marks moved (click suppressed)", () => {
    const m = createPointerMachine();
    m.down(down({ viewStart: 0, viewEnd: 1000, minTs: 0, maxTs: 10_000 }));
    const cmds = m.move({ clientX: 200, nsAtClamped: 0, drawW: 100 });
    expect(m.moved()).toBe(true);
    const pan = cmd(cmds, "pan");
    // dx=+100px over drawW=100 => shift one full view-duration LEFT (earlier).
    expect(pan).toMatchObject({ viewStart: 0, viewEnd: 1000 }); // clamped at minTs
  });

  it("up ends the pan: clear-drag + reset cursor", () => {
    const m = createPointerMachine();
    m.down(down());
    m.move({ clientX: 140, nsAtClamped: 5_400, drawW: 1000 });
    const cmds = m.up();
    expect(m.phase()).toBe("idle");
    expect(cmd(cmds, "clear-drag")).toBeDefined();
    expect(cmd(cmds, "cursor")?.cursor).toBe("");
    // A pan never commits a region.
    expect(cmd(cmds, "commit-region")).toBeUndefined();
  });

  it("moved() resets on the next down()", () => {
    const m = createPointerMachine();
    m.down(down());
    m.move({ clientX: 300, nsAtClamped: 9_000, drawW: 1000 });
    m.up();
    expect(m.moved()).toBe(true);
    m.down(down());
    expect(m.moved()).toBe(false);
  });
});

describe("createPointerMachine - region/zoom select commit", () => {
  it("moved region select commits [min,max] on up", () => {
    const m = createPointerMachine();
    m.down(down({ shiftKey: true, nsAt: 6_000 }));
    m.move({ clientX: 40, nsAtClamped: 2_000, drawW: 1000 }); // dx = -60px, moved
    expect(m.moved()).toBe(true);
    const cmds = m.up();
    const region = cmd(cmds, "commit-region");
    expect(region).toEqual({ type: "commit-region", kind: "region-select", startNs: 2_000, endNs: 6_000 });
  });

  it("zoom select commits kind zoom-select", () => {
    const m = createPointerMachine();
    m.down(down({ altKey: true, nsAt: 1_000 }));
    m.move({ clientX: 400, nsAtClamped: 8_000, drawW: 1000 });
    const region = cmd(m.up(), "commit-region");
    expect(region).toMatchObject({ kind: "zoom-select", startNs: 1_000, endNs: 8_000 });
  });

  it("UNMOVED shift press commits NOTHING (falls through to click select)", () => {
    const m = createPointerMachine();
    m.down(down({ shiftKey: true }));
    const cmds = m.up();
    expect(cmd(cmds, "commit-region")).toBeUndefined();
    expect(m.moved()).toBe(false); // the click handler runs the task select
    expect(cmd(cmds, "clear-drag")).toBeDefined();
  });

  it("select move rewrites the drag snapshot with the live curNs", () => {
    const m = createPointerMachine();
    m.down(down({ shiftKey: true, nsAt: 3_000 }));
    const drag = cmd(m.move({ clientX: 130, nsAtClamped: 7_777, drawW: 1000 }), "set-drag")?.drag;
    expect(drag).toMatchObject({ kind: "region-select", startNs: 3_000, curNs: 7_777, moved: true });
  });
});

describe("createPointerMachine - cancel paths", () => {
  it("cancel during a pan clears the drag and resets moved", () => {
    const m = createPointerMachine();
    m.down(down());
    m.move({ clientX: 300, nsAtClamped: 9_000, drawW: 1000 });
    const cmds = m.cancel();
    expect(m.phase()).toBe("idle");
    expect(cmd(cmds, "clear-drag")).toBeDefined();
    expect(cmd(cmds, "cursor")?.cursor).toBe("");
    expect(m.moved()).toBe(false);
  });

  it("cancel during a select clears the drag", () => {
    const m = createPointerMachine();
    m.down(down({ shiftKey: true }));
    expect(cmd(m.cancel(), "clear-drag")).toBeDefined();
    expect(m.phase()).toBe("idle");
  });

  it("cancel/move/up while idle are no-ops", () => {
    const m = createPointerMachine();
    expect(m.cancel()).toEqual([]);
    expect(m.move({ clientX: 10, nsAtClamped: 0, drawW: 100 })).toEqual([]);
    expect(m.up()).toEqual([]);
  });
});

describe("panWindowByPixels (H6 pan math, viewer.html:5286-5300)", () => {
  it("drags right => earlier window (negative shift), duration preserved", () => {
    const w = panWindowByPixels(1_000, 2_000, 100, 100, 0, 10_000);
    // nsPerPx = 1000/100 = 10; shift = -100*10 = -1000
    expect(w).toEqual({ viewStart: 0, viewEnd: 1_000 });
  });

  it("clamps at maxTs, carrying the overflow back to keep the width", () => {
    const w = panWindowByPixels(9_000, 10_000, -1000, 100, 0, 10_000);
    // shift = +10000 -> would exceed maxTs; edge-carry keeps duration 1000
    expect(w).toEqual({ viewStart: 9_000, viewEnd: 10_000 });
  });

  it("degenerate drawW <= 0 returns the anchor unchanged", () => {
    expect(panWindowByPixels(3, 7, 50, 0, 0, 10)).toEqual({ viewStart: 3, viewEnd: 7 });
  });
});
