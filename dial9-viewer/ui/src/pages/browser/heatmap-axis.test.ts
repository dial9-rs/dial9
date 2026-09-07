import { describe, expect, it } from "vitest";
import { axisTicks, cursorPlacement, tickTarget, timestampAt } from "./heatmap-axis.js";
import type { TimeDomain } from "./state.js";

const utc = (
  y: number,
  mo: number,
  d: number,
  h: number,
  mi = 0,
  s = 0,
): number => Date.UTC(y, mo - 1, d, h, mi, s) / 1000;

/** 2026-01-15 10:00:00Z + 10 minutes: one calendar day, 120s-aligned. */
const TEN_MINUTES: TimeDomain = {
  tMin: utc(2026, 1, 15, 10),
  tMax: utc(2026, 1, 15, 10, 10),
};

describe("tickTarget", () => {
  it("aims at one tick per 130px within 2..8", () => {
    expect(tickTarget(800)).toBe(6);
    // Narrow and very wide plots are clamped rather than degenerating into
    // a single tick or an unreadable comb.
    expect(tickTarget(100)).toBe(2);
    expect(tickTarget(0)).toBe(2);
    expect(tickTarget(4000)).toBe(8);
  });
});

describe("axisTicks", () => {
  // The point of the fix: ticks land on round wall-clock instants, so the
  // gridlines they anchor can be read as times instead of as an arbitrary
  // even division of the pane.
  it("places ticks on round times, aligned to the step", () => {
    const ticks = axisTicks(TEN_MINUTES, 800, false);

    expect(ticks.length).toBeGreaterThan(1);
    const step = ticks[1]!.t - ticks[0]!.t;
    expect(step).toBe(120);
    expect(ticks.every((tick) => tick.t % step === 0)).toBe(true);
    expect(ticks.length).toBeLessThanOrEqual(tickTarget(800));
  });

  it("maps each tick time to its pixel column", () => {
    const W = 800;
    const ticks = axisTicks(TEN_MINUTES, W, false);
    const span = TEN_MINUTES.tMax - TEN_MINUTES.tMin;

    expect(ticks[0]!.x).toBe(0);
    for (const tick of ticks) {
      expect(tick.x).toBeCloseTo(((tick.t - TEN_MINUTES.tMin) / span) * W, 6);
      expect(tick.x).toBeGreaterThanOrEqual(0);
      expect(tick.x).toBeLessThanOrEqual(W);
    }
  });

  // Within one day only the leftmost tick needs the date; repeating it on
  // every tick would be noise.
  it("dates the leftmost tick and leaves the rest time-only", () => {
    const ticks = axisTicks(TEN_MINUTES, 800, false);

    expect(ticks[0]!.label).toBe("2026-01-15 10:00:00");
    expect(ticks[1]!.label).toBe("10:02:00");
    expect(ticks.slice(1).every((tick) => !tick.label.includes("2026"))).toBe(true);
  });

  // A bare HH:MM:SS cannot say which day it means once the window straddles
  // midnight, so every tick carries its date there.
  it("dates every tick when the window crosses a day boundary", () => {
    const ticks = axisTicks(
      { tMin: utc(2026, 1, 15, 23, 30), tMax: utc(2026, 1, 16, 0, 30) },
      800,
      false,
    );

    expect(ticks.length).toBeGreaterThan(1);
    expect(ticks.every((tick) => /^\d{4}-\d{2}-\d{2} /.test(tick.label))).toBe(true);
    expect(ticks.some((tick) => tick.label.startsWith("2026-01-15"))).toBe(true);
    expect(ticks.some((tick) => tick.label.startsWith("2026-01-16"))).toBe(true);
  });

  it("labels in local time when the TZ toggle is on local", () => {
    const [tick] = axisTicks(TEN_MINUTES, 800, true);
    const local = new Date(TEN_MINUTES.tMin * 1000);
    const pad = (n: number) => String(n).padStart(2, "0");

    expect(tick!.label).toBe(
      `${local.getFullYear()}-${pad(local.getMonth() + 1)}-${pad(local.getDate())} ` +
        `${pad(local.getHours())}:${pad(local.getMinutes())}:${pad(local.getSeconds())}`,
    );
  });

  // A zero-width domain would make the time->x map a 0/0. Pinning the lone
  // tick to the left edge keeps a NaN out of the canvas and the tick's CSS
  // `left`, either of which silently drops the axis.
  it("pins the lone tick to the left edge for a degenerate domain", () => {
    const t = utc(2026, 1, 15, 10);

    for (const domain of [
      { tMin: t, tMax: t },
      { tMin: t, tMax: t - 60 },
    ]) {
      const ticks = axisTicks(domain, 800, false);
      expect(ticks).toHaveLength(1);
      expect(ticks[0]!.x).toBe(0);
      expect(ticks[0]!.label).toBe("2026-01-15 10:00:00");
    }
  });
});

describe("timestampAt", () => {
  it("reads the time under the pointer across the plot", () => {
    expect(timestampAt(0, TEN_MINUTES, 800, false)).toBe("2026-01-15 10:00:00");
    expect(timestampAt(400, TEN_MINUTES, 800, false)).toBe("2026-01-15 10:05:00");
    expect(timestampAt(800, TEN_MINUTES, 800, false)).toBe("2026-01-15 10:10:00");
  });
});

describe("cursorPlacement", () => {
  it("centers the label on the pointer away from the edges", () => {
    expect(cursorPlacement(400, 800, 120)).toStrictEqual({
      lineLeft: 400,
      labelLeft: 400,
    });
  });

  // The label is center-anchored, so without the clamp its left half hangs
  // off the plot at x=0 and its right half at x=W.
  it("keeps the label inside the plot at both edges", () => {
    expect(cursorPlacement(0, 800, 120).labelLeft).toBe(60);
    expect(cursorPlacement(800, 800, 120).labelLeft).toBe(740);
  });

  it("rounds the cursor line to a whole pixel column", () => {
    expect(cursorPlacement(123.7, 800, 120).lineLeft).toBe(124);
  });

  // No offset fits a label wider than the plot; centering clips both ends
  // equally instead of pushing one entirely out of view.
  it("centers a label wider than the plot", () => {
    expect(cursorPlacement(10, 100, 200).labelLeft).toBe(50);
  });
});

// Guard the markup the hover readout binds to: a rename would otherwise
// surface only as a boot-time crash in queryEls().
describe("heatmap cursor markup", () => {
  it("keeps the hover-readout elements in index.html", async () => {
    const { readFile } = await import("node:fs/promises");
    const html = await readFile(new URL("../../../index.html", import.meta.url), "utf8");

    expect(html).toContain('id="heatmap-cursor"');
    expect(html).toContain('id="heatmap-cursor-label"');
  });
});
