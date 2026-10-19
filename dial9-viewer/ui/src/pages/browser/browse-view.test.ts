// Wiring tests for the browse painter: that the timeline actually renders
// the round-time axis model (#631), not just that the model computes one.

import { afterEach, describe, expect, it, vi } from "vitest";
import { densityColor } from "../../lib/canvas/heatmap.js";
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
  readonly fills: { y: number; height: number; color: string }[] = [];
  fillStyle = "";
  strokeStyle = "";
  lineWidth = 1;
  private dash: number[] = [];
  private readonly dashStack: number[][] = [];
  private from: { x: number; y: number } | null = null;
  private to: { x: number; y: number } | null = null;

  setTransform(): void {}
  clearRect(): void {}
  fillRect(_x: number, y: number, _width: number, height: number): void {
    this.fills.push({ y, height, color: this.fillStyle });
  }
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
  value = "";
  readonly attributes = new Map<string, string>();
  className = "";
  style: Record<string, string> = {};
  children: FakeElement[] = [];
  title = "";
  clientWidth = 0;
  clientHeight = 0;
  scrollTop = 0;
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
  let width = 0;
  let height = 0;
  Object.defineProperties(canvas, {
    width: { configurable: true, get: () => width, set: (value: number) => { width = value; } },
    height: { configurable: true, get: () => height, set: (value: number) => { height = value; } },
  });
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
  const hostFilter = new FakeElement();
  const clearFilter = new FakeElement();
  const hostCount = new FakeElement();
  const body = new FakeElement();
  body.clientWidth = 1020;
  body.clientHeight = 260;
  const els = {
    browseWarning: new FakeElement(),
    browseStatus: new FakeElement(),
    heatmapView: new FakeElement(),
    heatmapResetZoom: new FakeElement(),
    heatmapShowAllHosts: showAllHosts,
    heatmapHostFilter: hostFilter,
    heatmapHostClear: clearFilter,
    heatmapHostCount: hostCount,
    heatmapLabels: labels,
    heatmapBody: body,
    heatmapLabelResizer: new FakeElement(),
    heatmapPlot: plot,
    heatmapCanvas: canvas,
    heatmapAxis: axis,
  } as unknown as BrowserEls;

  const store = createBrowserStore();
  const actions = createActions(store, els);
  mountBrowseView({ store, els, actions });
  return { store, ctx, axis, canvas, plot, labels, showAllHosts, body, hostFilter, clearFilter, hostCount };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("host focus controls", () => {
  it("filters host names case-insensitively, clears stale selections, and restores the list", async () => {
    const { store, labels, hostFilter, clearFilter, hostCount } = setup();
    const rows = [row(), { ...row(), host: "Host-2", label: "api / Host-2" }];
    store.update("browse", { rows, allRows: rows, domain: DOMAIN, heatmapVisible: true,
      selection: { keys: ["old.bin"], bytes: 1, t0: 1, t1: 2, rows: [0, 0] } });
    await flushStore();
    hostFilter.value = "host-2";
    hostFilter.dispatchEvent(new Event("input"));
    await flushStore();
    expect(store.getState().browse.rows).toEqual([rows[1]]);
    expect(store.getState().browse.selection).toBeNull();
    expect(labels.children).toHaveLength(1);
    expect(hostCount.textContent).toBe("1 of 2 rows");

    clearFilter.click();
    await flushStore();
    expect(store.getState().browse.rows).toBe(rows);
    expect(hostFilter.value).toBe("");
    expect(hostCount.textContent).toBe("2 of 2 rows");
  });

  it("matches raw paths and leaves the filter usable when nothing matches", async () => {
    const { store, labels, hostFilter, canvas, hostCount } = setup();
    const rows = toRows(toSegments([
      { key: "traces/2026-10-15/1200/api/region/Host-1/boot/1792065600-0.bin.gz", size: 10 },
    ]));
    store.update("browse", { rows, allRows: rows, domain: DOMAIN, heatmapVisible: true });
    await flushStore();
    hostFilter.value = "HOST-1";
    hostFilter.dispatchEvent(new Event("input"));
    await flushStore();
    expect(labels.children).toHaveLength(1);

    hostFilter.value = "no-match";
    hostFilter.dispatchEvent(new Event("input"));
    await flushStore();
    expect(labels.children).toHaveLength(0);
    expect(canvas.height).toBe(0);
    expect(hostCount.textContent).toBe("0 of 1 rows · No matching hosts");
    expect(store.getState().browse.heatmapVisible).toBe(true);
  });

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
  it("bounds the canvas and painted rows to the viewport for thousands of hosts", async () => {
    const { store, ctx, canvas, plot, body } = setup();
    const rows = Array.from({ length: 2000 }, (_, i) => ({
      ...row(), host: `h${i}`, label: `api / h${i}`,
    }));
    vi.stubGlobal("window", { devicePixelRatio: 2 });
    store.update("browse", { rows, domain: DOMAIN, heatmapVisible: true });
    await flushStore();

    expect(canvas.height).toBe(body.clientHeight * 4);
    expect(plot.style["height"]).toBe(`${rows.length * ROW_H}px`);
    expect(ctx.fills.filter((fill) => fill.height === ROW_H)).toHaveLength(20);

    ctx.fills.length = 0;
    body.scrollTop = 1500 * ROW_H + 7;
    body.dispatchEvent(new Event("scroll"));
    await flushStore();

    expect(canvas.height).toBeLessThanOrEqual((3 * body.clientHeight + ROW_H) * 2);
    expect(canvas.style["top"]).toBe(`${1490 * ROW_H}px`);
    const strips = ctx.fills.filter((fill) => fill.height === ROW_H);
    expect(strips).toHaveLength(31);
    expect(strips[0]!.y).toBe(0);
    expect(strips.at(-1)!.y).toBe(780);
  });

  it("scrolls within the painted buffer without repainting or resetting the bitmap", async () => {
    const { store, ctx, canvas, body } = setup();
    const rows = Array.from({ length: 100 }, (_, i) => ({
      ...row(), host: `h${i}`, label: `api / h${i}`,
    }));
    store.update("browse", { rows, domain: DOMAIN, heatmapVisible: true });
    await flushStore();
    const width = vi.spyOn(canvas, "width", "set");
    const height = vi.spyOn(canvas, "height", "set");
    ctx.fills.length = 0;
    body.scrollTop = 100;
    body.dispatchEvent(new Event("scroll"));
    await flushStore();

    expect(ctx.fills).toHaveLength(0);
    expect(width).not.toHaveBeenCalled();
    expect(height).not.toHaveBeenCalled();

    body.scrollTop = 900;
    body.dispatchEvent(new Event("scroll"));
    await flushStore();
    expect(ctx.fills.length).toBeGreaterThan(0);
    expect(Number.parseInt(canvas.style["top"]!)).toBeLessThanOrEqual(body.scrollTop);
    expect(Number.parseInt(canvas.style["top"]!) + Number.parseInt(canvas.style["height"]!))
      .toBeGreaterThanOrEqual(body.scrollTop + body.clientHeight);
  });

  it("keeps density colors comparable to offscreen hosts while scrolling", async () => {
    const { store, ctx, body } = setup();
    body.clientHeight = ROW_H;
    const rows = [1, 10, 1].map((size, i) => {
      const segment = {
        key: `h${i}/trace.bin`, start: DOMAIN.tMin, end: DOMAIN.tMax, size,
        service: "api", host: `h${i}`, bootId: "", layout: "known" as const,
      };
      return { ...row(), host: segment.host, label: `api / h${i}`,
        segments: [segment], tiled: [segment], totalBytes: size };
    });
    store.update("browse", { rows, domain: DOMAIN, heatmapVisible: true });
    await flushStore();
    const colors = (y: number) => ctx.fills
      .filter((fill) => fill.y === y && fill.height === ROW_H && !["#191932", "#20203e"].includes(fill.color))
      .map((fill) => fill.color);
    const firstColors = colors(0);
    expect(firstColors.length).toBeGreaterThan(0);

    ctx.fills.length = 0;
    body.scrollTop = 2 * ROW_H;
    body.dispatchEvent(new Event("scroll"));
    await flushStore();
    expect(colors(ROW_H)).toEqual(firstColors);
    expect(colors(ROW_H)).not.toContain(densityColor(1));
  });

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
