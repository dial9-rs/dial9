// Wiring tests for the browse painter: that the timeline actually renders
// the round-time axis model (#631), not just that the model computes one.

import { afterEach, describe, expect, it, vi } from "vitest";
import { ROW_H, type BrowserActions } from "./actions.js";
import { mountBrowseView } from "./browse-view.js";
import type { BrowserEls } from "./dom.js";
import { axisTicks } from "./heatmap-axis.js";
import { createBrowserStore, type HeatmapRow, type TimeDomain } from "./state.js";

/** A vertical line the painter stroked, as (x, y0)->(x, y1). */
interface StrokedLine {
  x: number;
  y0: number;
  y1: number;
  dashed: boolean;
}

/**
 * A 2D context that records the vertical strokes it is asked to draw. Only
 * the calls browse-view makes are implemented; anything it does not record is
 * a no-op, so a fill-heavy paint stays cheap.
 */
class RecordingContext {
  readonly lines: StrokedLine[] = [];
  fillStyle = "";
  strokeStyle = "";
  lineWidth = 1;
  private dash: number[] = [];
  private readonly dashStack: number[][] = [];
  private from: { x: number; y: number } | null = null;
  private to: { x: number; y: number } | null = null;

  setTransform(): void {}
  clearRect(): void {}
  fillRect(): void {}
  rect(): void {}
  clip(): void {}
  save(): void {
    this.dashStack.push([...this.dash]);
  }
  restore(): void {
    this.dash = this.dashStack.pop() ?? [];
  }
  setLineDash(dash: number[]): void {
    this.dash = dash;
  }
  beginPath(): void {
    this.from = null;
    this.to = null;
  }
  moveTo(x: number, y: number): void {
    this.from = { x, y };
  }
  lineTo(x: number, y: number): void {
    this.to = { x, y };
  }
  stroke(): void {
    const { from, to } = this;
    // Only vertical segments are of interest (gridlines, boot dividers);
    // the gap hatching draws diagonals.
    if (from && to && from.x === to.x) {
      this.lines.push({
        x: from.x,
        y0: from.y,
        y1: to.y,
        dashed: this.dash.length > 0,
      });
    }
  }
}

class FakeElement {
  className = "";
  style: Record<string, string> = {};
  children: FakeElement[] = [];
  title = "";
  clientWidth = 0;
  private text = "";

  get textContent(): string {
    return this.text + this.children.map((c) => c.textContent).join("");
  }
  set textContent(value: string | null) {
    this.text = value ?? "";
    this.children = [];
  }
  append(...children: FakeElement[]): void {
    this.children.push(...children);
  }
  appendChild(child: FakeElement): FakeElement {
    this.children.push(child);
    return child;
  }
}

const W = 800;

/** 2026-01-15 10:00:00Z + 10 minutes. */
const DOMAIN: TimeDomain = {
  tMin: Date.UTC(2026, 0, 15, 10, 0, 0) / 1000,
  tMax: Date.UTC(2026, 0, 15, 10, 10, 0) / 1000,
};

function row(): HeatmapRow {
  return {
    service: "api",
    host: "h1",
    label: "api / h1",
    segments: [],
    totalBytes: 0,
    tiled: [],
    gaps: [],
  } as unknown as HeatmapRow;
}

async function flushStore(): Promise<void> {
  await Promise.resolve();
}

function setup() {
  const ctx = new RecordingContext();
  const canvas = new FakeElement() as FakeElement & {
    width: number;
    height: number;
    getContext(): RecordingContext;
  };
  canvas.width = 0;
  canvas.height = 0;
  canvas.getContext = () => ctx;

  const plot = new FakeElement();
  plot.clientWidth = W;
  const axis = new FakeElement();

  vi.stubGlobal("document", {
    createElement: () => new FakeElement(),
    createTextNode: (text: string) => {
      const node = new FakeElement();
      node.textContent = text;
      return node;
    },
    documentElement: {},
  });
  vi.stubGlobal("window", { devicePixelRatio: 1 });
  vi.stubGlobal("getComputedStyle", () => ({
    // Exercise the real custom-property path rather than the 220 fallback.
    getPropertyValue: () => "220px",
  }));

  const els = {
    browseWarning: new FakeElement(),
    browseStatus: new FakeElement(),
    heatmapView: new FakeElement(),
    heatmapResetZoom: new FakeElement(),
    heatmapLabels: new FakeElement(),
    heatmapPlot: plot,
    heatmapCanvas: canvas,
    heatmapAxis: axis,
  } as unknown as BrowserEls;

  const store = createBrowserStore();
  mountBrowseView({ store, els, actions: {} as unknown as BrowserActions });
  return { store, ctx, axis, canvas };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("browse timeline painter", () => {
  it("labels the axis with the round-time tick model", async () => {
    const { store, axis } = setup();
    store.update("browse", { rows: [row()], domain: DOMAIN, heatmapVisible: true });
    await flushStore();

    const expected = axisTicks(DOMAIN, W, false);
    expect(expected.length).toBeGreaterThan(1);
    expect(axis.children.map((tick) => tick.textContent)).toStrictEqual(
      expected.map((tick) => tick.label),
    );
    // Ticks land on round wall-clock instants (every 2 minutes here), which
    // an even division of the pane would not produce.
    expect(axis.children.map((tick) => tick.textContent)).toStrictEqual([
      "2026-01-15 10:00:00",
      "10:02:00",
      "10:04:00",
      "10:06:00",
      "10:08:00",
      "10:10:00",
    ]);
  });

  it("offsets the tick labels past the host-label column", async () => {
    const { store, axis } = setup();
    store.update("browse", { rows: [row()], domain: DOMAIN, heatmapVisible: true });
    await flushStore();

    const expected = axisTicks(DOMAIN, W, false);
    expect(axis.children.map((tick) => tick.style["left"])).toStrictEqual(
      expected.map((tick) => 220 + tick.x + "px"),
    );
  });

  // The gridlines are what make the tick labels readable against the rows;
  // they must span the full canvas height at exactly the tick columns.
  it("draws a full-height gridline at every axis tick", async () => {
    const { store, ctx } = setup();
    const rows = [row(), row()];
    store.update("browse", { rows, domain: DOMAIN, heatmapVisible: true });
    await flushStore();

    const expected = axisTicks(DOMAIN, W, false);
    const gridlines = ctx.lines.filter((line) => !line.dashed);
    expect(gridlines.map((line) => line.x)).toStrictEqual(
      expected.map((tick) => Math.round(tick.x) + 0.5),
    );

    const H = rows.length * ROW_H;
    for (const line of gridlines) {
      expect(line.y0).toBe(0);
      expect(line.y1).toBeGreaterThan(0);
      expect(line.y1).toBe(H);
    }
  });

  it("relabels in local time when the TZ toggle flips", async () => {
    const { store, axis } = setup();
    store.update("browse", { rows: [row()], domain: DOMAIN, heatmapVisible: true });
    await flushStore();
    const utcLabels = axis.children.map((tick) => tick.textContent);

    store.update("ui", { useLocalTz: true });
    await flushStore();

    expect(axis.children.map((tick) => tick.textContent)).toStrictEqual(
      axisTicks(DOMAIN, W, true).map((tick) => tick.label),
    );
    // Sanity: this harness runs in a TZ where the two differ, or the
    // assertion above proves nothing.
    if (new Date().getTimezoneOffset() !== 0) {
      expect(axis.children.map((tick) => tick.textContent)).not.toStrictEqual(utcLabels);
    }
  });
});
