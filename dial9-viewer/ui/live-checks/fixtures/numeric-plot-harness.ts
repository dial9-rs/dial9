import { html, nothing, render, type RootPart } from "lit-html";
import { renderNumericPlot } from "../../src/pages/span-explorer/numeric-plot.js";
import { SpanTailAnalysis } from "../../src/pages/span-explorer/tail-analysis.js";
import { readScope, readTailView } from "../../src/pages/span-explorer/scope.js";
import type { SpanTypeStats } from "../../src/lib/trace/index.js";
import type { TailRecord, TailResponse, TailSignal } from "../../src/lib/trace/span-tail-analysis.js";
import "../../src/pages/span-explorer/tail-analysis.css";

interface DrawOptions {
  plots?: number;
  points?: number;
  large?: boolean;
  noDensity?: boolean;
  oldReport?: boolean;
  elapsedScale?: "linear" | "log";
  minMs?: number;
  maxMs?: number;
  fields?: Pick<TailRecord, "attributes" | "attributesOmitted">;
  fieldsByPoint?: readonly Pick<TailRecord, "attributes" | "attributesOmitted">[];
  signal?: Partial<TailSignal>;
}
export interface NumericPlotHarness {
  draw: (options?: DrawOptions) => void;
  clear: () => void;
  connect: (connected: boolean) => void;
  queuedRemoval: () => void;
  mountReport: () => void;
  replaceReport: () => void;
  clearReport: (sameHost?: boolean) => void;
  reportResponse: () => TailResponse;
  readonly resources: { observers: number; hideTimers: number };
  readonly jumps: TailRecord[];
  readonly previousReportEmpty: boolean;
}
declare global {
  interface Window { numericPlotHarness: NumericPlotHarness; }
}

// Observe resource lifetimes through the native browser dependencies.
const observers = new Set<ResizeObserver>();
const NativeObserver = window.ResizeObserver;
window.ResizeObserver = class extends NativeObserver {
  override observe(...args: Parameters<ResizeObserver["observe"]>) {
    observers.add(this);
    super.observe(...args);
  }
  override disconnect() { observers.delete(this); super.disconnect(); }
};
const timers = new Set<number>();
const browserTimers = window as unknown as {
  setTimeout: (callback: TimerHandler, delay?: number, ...args: unknown[]) => number;
  clearTimeout: (id?: number) => void;
};
const setTimeout = browserTimers.setTimeout.bind(window), clearTimeout = browserTimers.clearTimeout.bind(window);
browserTimers.setTimeout = (callback: TimerHandler, delay?: number, ...args: unknown[]) => {
  if (delay !== 180 || typeof callback !== "function") return setTimeout(callback, delay, ...args);
  const id = setTimeout(() => { timers.delete(id); callback(...args); }, delay);
  timers.add(id);
  return id;
};
browserTimers.clearTimeout = id => { if (id != null) timers.delete(id); clearTimeout(id); };

const plots = document.querySelector<HTMLElement>("#plots")!;
const reports = document.querySelector<HTMLElement>("#reports")!;
const jumps: TailRecord[] = [];
let root: RootPart;
let reportHost: HTMLElement | null = null;
let previousReport: HTMLElement | null = null;
const analysis = new SpanTailAnalysis();
const context: NonNullable<Parameters<SpanTailAnalysis["mount"]>[1]> = {
  spanType: { span_type_uid: "request-uid", name: "request" } as SpanTypeStats,
  scope: readScope(new URLSearchParams("api=1&data_dir=fixture")),
  view: { startNs: null, endNs: null, selectedUid: "request-uid", bandMinNs: null,
    bandMaxNs: null, attrFilters: [], maxFiles: null },
  analysis: readTailView(new URLSearchParams()),
  headers: {}, onClearBand: () => {}, onBand: () => {},
  onAnalysisChange: (next, clearBand) => {
    context.analysis = next;
    if (clearBand) context.view = { ...context.view, bandMinNs: null, bandMaxNs: null };
    analysis.mount(reportHost, context);
  },
};
function signal(options: DrawOptions = {}): TailSignal {
  const count = options.points ?? 3;
  return {
    key: "work_time", unit: "ns", typical: 1e6, slow: 8e6, typicalCount: 50, slowCount: 20,
    typicalCoverage: 1, slowCoverage: 1, separation: .8, correlation: .8, correlationCount: 100,
    duplicatesLatency: false, filesCompared: 1, filesAgree: 1,
    ...(options.oldReport ? {} : { valueAxis: { unit: "ms", multiplier: 1e-6, minimum: 0, maximum: 12 } }),
    ...(options.noDensity || options.oldReport ? {} : { density: {
      elapsedEdgesMs: [0, 1, 100], valueEdges: [0, 6, 12],
      counts: [9000, 500, 0, 500], count: 10000, maxCount: 9000,
    } }),
    points: Array.from({ length: count }, (_, index) => {
      const elapsed = count === 1 ? 1 : 100 ** (index / (count - 1));
      return [elapsed, (1 + 10 * index / Math.max(1, count - 1)) * 1e6, {
        elapsedMs: elapsed, insideMs: null, startNs: String(index + 1), endNs: "100000001",
        sourceUrl: "demo.bin", requestId: `request-${index}`,
        ...(options.fieldsByPoint?.[index] ?? options.fields
          ?? { attributes: [["status", "001"], ["status", "false"]], attributesOmitted: 0 }),
      }];
    }),
    ...options.signal,
  };
}
function draw(options: DrawOptions = {}) {
  const value = signal(options);
  root = render(html`${Array.from({ length: options.plots ?? 1 }, () => html`
    <section class="detail-panel">${options.large ? html`<dialog class="tail-plot-dialog">
      ${plot(value, true, options)}</dialog>` : plot(value, false, options)}</section>`)}`, plots);
  plots.querySelectorAll("dialog").forEach(dialog => { if (!dialog.open) dialog.showModal(); });
}
function plot(value: TailSignal, large: boolean, options: DrawOptions) {
  return renderNumericPlot(value, { signals: [value], boundaryMs: options.minMs ?? 10, percentile: 99,
    minMs: options.minMs ?? null, maxMs: options.maxMs ?? null, large,
    elapsedScale: options.elapsedScale ?? "log", onJump: row => jumps.push(row) });
}
function replaceReport() {
  previousReport = reportHost;
  // The outer Lit root cannot dispose the independently rendered report root.
  render(nothing, reports);
  render(html`<section class="detail-panel"><div class="span-tail-mount"></div></section>`, reports);
  reportHost = reports.querySelector<HTMLElement>(".span-tail-mount")!;
  analysis.mount(reportHost, context);
}
window.numericPlotHarness = {
  draw, clear: () => { root = render(nothing, plots); },
  connect: connected => root.setConnected(connected),
  queuedRemoval: () => { draw(); root = render(nothing, plots); },
  mountReport: () => { root = render(nothing, plots); replaceReport(); },
  replaceReport,
  clearReport: (sameHost = false) => {
    previousReport = reportHost;
    analysis.mount(sameHost ? reportHost : null, null);
  },
  reportResponse: () => ({
    filesUsed: 1, filesMatched: 1, truncated: false, fieldsTruncated: false,
    report: {
      count: 10000, typicalCount: 50, slowCount: 20, medianMs: 1, boundaryMs: 10,
      typicalElapsedMs: 1, slowElapsedMs: 12, typicalInsideMs: null, slowInsideMs: null,
      typicalOutsideMs: null, slowOutsideMs: null, typicalAttributed: 0, slowAttributed: 0,
      typicalComposition: null, slowComposition: null, groups: [], mixChanges: [], examples: [],
      histograms: [], signals: [signal()],
    },
  }),
  get resources() { return { observers: observers.size, hideTimers: timers.size }; },
  jumps,
  get previousReportEmpty() { return previousReport?.textContent === ""; },
};
draw();
