// Wiring tests for the browse painter: that the timeline actually renders
// the round-time axis model (#631), not just that the model computes one.

import { afterEach, describe, expect, it, vi } from "vitest";
import { createActions, ROW_H } from "./actions.js";
import { mountBrowseView } from "./browse-view.js";
import type { BrowserEls } from "./dom.js";
import { axisTicks } from "./heatmap-axis.js";
import { toRows, toSegments } from "./segments.js";
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
  font = "";
  measureText(text: string): { width: number } {
    return { width: text.length * 8 };
  }
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

class FakeElement extends EventTarget {
  tagName = "";
  type = "";
  readonly attributes = new Map<string, string>();
  className = "";
  style: Record<string, string> = {};
  children: FakeElement[] = [];
  title = "";
  clientWidth = 0;
  offsetLeft = 0;
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
  setAttribute(name: string, value: string): void {
    this.attributes.set(name, value);
  }
  focus(): void {
    Object.assign(document, { activeElement: this });
  }
  click(): void {
    this.dispatchEvent(new Event("click"));
  }
  querySelector(selector: string): FakeElement | null {
    return this.children.find((child) => `.${child.className}` === selector) ?? null;
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
    createElement: (tag: string) => tag === "canvas" ? canvas : Object.assign(new FakeElement(), { tagName: tag }),
    createTextNode: (text: string) => {
      const node = new FakeElement();
      node.textContent = text;
      return node;
    },
    documentElement: { style: { setProperty: vi.fn() } },
  });
  vi.stubGlobal("window", { devicePixelRatio: 1 });
  vi.stubGlobal("getComputedStyle", () => ({
    font: "12px sans-serif",
    // Exercise the real custom-property path rather than the 220 fallback.
    getPropertyValue: () => "220px",
  }));

  const labels = new FakeElement();
  const showAllHosts = new FakeElement();
  const els = {
    browseWarning: new FakeElement(),
    browseStatus: new FakeElement(),
    heatmapView: new FakeElement(),
    heatmapResetZoom: new FakeElement(),
    heatmapShowAllHosts: showAllHosts,
    heatmapLabels: labels,
    heatmapBody: { clientWidth: 1020 },
    heatmapPlot: plot,
    heatmapCanvas: canvas,
    heatmapAxis: axis,
  } as unknown as BrowserEls;

  const store = createBrowserStore();
  const actions = createActions(store, els);
  mountBrowseView({ store, els, actions });
  return { store, ctx, axis, canvas, plot, labels, showAllHosts };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("host focus controls", () => {
  it("uses named native buttons and retains keyboard focus through focus and restore", async () => {
    const { store, labels, showAllHosts, canvas } = setup();
    const rows = [row(), { ...row(), host: "h2", label: "api / h2" }];
    store.update("browse", { rows, domain: DOMAIN, heatmapVisible: true });
    await flushStore();
    expect(showAllHosts.style["display"]).toBe("none");

    const target = labels.children[1]!;
    expect(target.tagName).toBe("button");
    expect(target.type).toBe("button");
    expect(target.attributes.get("aria-label")).toBe("Focus on api / h2");
    expect(target.title).toBe("api / h2");
    target.focus();
    target.click();
    await flushStore();

    expect(store.getState().browse.rows).toEqual([rows[1]]);
    expect(store.getState().browse.domain).toBe(DOMAIN);
    expect(labels.children).toHaveLength(1);
    expect(document.activeElement).toBe(labels.children[0]);
    expect(canvas.height).toBe(ROW_H);
    expect(showAllHosts.style["display"]).toBe("");

    showAllHosts.focus();
    showAllHosts.click();
    await flushStore();

    expect(store.getState().browse.rows).toBe(rows);
    expect(labels.children).toHaveLength(2);
    expect(document.activeElement).toBe(labels.children[1]);
    expect(canvas.height).toBe(2 * ROW_H);
    expect(showAllHosts.style["display"]).toBe("none");
  });

  it("keeps the focused row and aligned time axis when resized", async () => {
    const { store, labels, canvas, plot, axis } = setup();
    const rows = [row(), { ...row(), host: "h2", label: "api / h2" }];
    store.update("browse", { rows, domain: DOMAIN, heatmapVisible: true });
    await flushStore();
    labels.children[1]!.click();
    await flushStore();
    const focusedLabel = labels.children[0];

    plot.clientWidth = 500;
    plot.offsetLeft = 300;
    store.update("browse", { renderEpoch: 1 });
    await flushStore();

    expect(labels.children).toEqual([focusedLabel]);
    expect(canvas.width).toBe(500);
    expect(canvas.height).toBe(ROW_H);
    expect(store.getState().browse.domain).toBe(DOMAIN);
    expect(axis.children.map((tick) => tick.style["left"])).toEqual(
      axisTicks(DOMAIN, 500, false).map((tick) => `${300 + tick.x}px`),
    );
  });

  it("labels unknown layouts as raw paths, not invented service/host identities", async () => {
    const { store, labels } = setup();
    const segments = toSegments([
      { key: "unrecognized/path/1768471200-0.bin.gz", size: 10 },
      { key: "unrecognized/other/1768471200-0.bin.gz", size: 10 },
    ]);
    const rows = toRows(segments);
    store.update("browse", { segments, rows, domain: DOMAIN, heatmapVisible: true });
    await flushStore();
    const label = labels.children.find((child) => child.title === "unrecognized/path")!;
    expect(label.textContent).toBe("unrecognized/path");
    expect(label.tagName).toBe("div");
    expect(label.attributes.has("aria-label")).toBe(false);
    label.click();
    await flushStore();

    expect(labels.children).toHaveLength(2);
    expect(store.getState().browse.rows).toBe(rows);
    expect(store.getState().browse.unfocusedRows).toBeNull();
  });
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
      "2026/01/15 10:00:00 UTC",
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

  it("keeps round-time ticks aligned after resizing the label column", async () => {
    const { store, axis, plot } = setup();
    plot.offsetLeft = 228;
    store.update("browse", { rows: [row()], domain: DOMAIN, heatmapVisible: true });
    await flushStore();
    const ticks = axisTicks(DOMAIN, W, false);
    expect(axis.children.map((tick) => tick.style["left"])).toStrictEqual(
      ticks.map((tick) => 228 + tick.x + "px"),
    );

    plot.offsetLeft = 428;
    store.update("browse", { renderEpoch: store.getState().browse.renderEpoch + 1 });
    await flushStore();
    expect(axis.children.map((tick) => tick.style["left"])).toStrictEqual(
      ticks.map((tick) => 428 + tick.x + "px"),
    );
    expect(axis.children.map((tick) => tick.textContent)).toStrictEqual(
      ticks.map((tick) => tick.label),
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
