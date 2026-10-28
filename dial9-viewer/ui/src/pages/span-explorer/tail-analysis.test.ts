import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render } from "lit-html";
import { SpanTailAnalysis } from "./tail-analysis.js";
import { readScope, readTailView } from "./scope.js";
import type { SpanTypeStats } from "../../lib/trace/index.js";
import type { TailReport, TailResponse } from "../../lib/trace/span-tail-analysis.js";

vi.mock("lit-html", async (original) => ({
  ...await original<typeof import("lit-html")>(),
  render: vi.fn(),
}));

interface Template {
  strings: readonly string[];
  values: unknown[];
}

function isTemplate(value: unknown): value is Template {
  return value != null && typeof value === "object" && "strings" in value && "values" in value;
}

function text(value: unknown): string {
  if (Array.isArray(value)) return value.map(text).join("");
  if (!isTemplate(value)) return typeof value === "string" || typeof value === "number" ? String(value) : "";
  return value.strings.map((s, i) => s + text(value.values[i])).join("");
}

function click(value: unknown, label: string): () => void {
  if (Array.isArray(value)) {
    for (const child of value) {
      try { return click(child, label); } catch { /* Keep looking in sibling templates. */ }
    }
  } else if (isTemplate(value)) {
    for (let i = 0; i < value.values.length; i++) {
      const hasLabel = text({ strings: value.strings.slice(i + 1), values: value.values.slice(i + 1) })
        .split("</button>")[0]?.includes(label);
      if (value.strings[i]?.endsWith("@click=") && hasLabel) {
        return value.values[i] as () => void;
      }
      try { return click(value.values[i], label); } catch { /* Keep looking in sibling templates. */ }
    }
  }
  throw new Error(`No button labelled ${label}`);
}

function bindings(value: unknown, attribute: string): unknown[] {
  if (Array.isArray(value)) return value.flatMap(child => bindings(child, attribute));
  if (!isTemplate(value)) return [];
  return value.values.flatMap((child, i) => [
    ...(value.strings[i]?.endsWith(attribute) ? [child] : []),
    ...bindings(child, attribute),
  ]);
}

function report(overrides: Partial<TailReport> = {}): TailReport {
  return {
    count: 100, typicalCount: 50, slowCount: 20, medianMs: 1, boundaryMs: 10,
    typicalElapsedMs: 1, slowElapsedMs: 12, typicalInsideMs: null, slowInsideMs: null,
    typicalOutsideMs: null, slowOutsideMs: null, typicalAttributed: 0, slowAttributed: 0,
    typicalComposition: null, slowComposition: null, groups: [], mixChanges: [], examples: [],
    histograms: [{
      key: "cache_hit", count: 100, missingCount: 0, binEdgesMs: [0, 1, 10, 100], maxShare: 1,
      values: [
        { value: "true", count: 50, shares: [.9, .1, 0], medianMs: .5, p99Ms: 2 },
        { value: "false", count: 50, shares: [0, .5, .5], medianMs: 8, p99Ms: 80 },
      ],
    }],
    signals: [{
      key: "work_bytes", unit: "bytes", typical: 10, slow: 80, typicalCount: 50, slowCount: 20,
      typicalCoverage: 1, slowCoverage: 1, separation: .8, correlation: .8, correlationCount: 100,
      duplicatesLatency: false, filesCompared: 1, filesAgree: 1, points: [],
    }],
    ...overrides,
  };
}

function response(value: TailReport): Response {
  const body: TailResponse = { report: value, filesUsed: 1, filesMatched: 1, truncated: false, fieldsTruncated: false };
  return new Response(JSON.stringify(body), { status: 200 });
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(r => { resolve = r; });
  return { promise, resolve };
}

function setup({ mount = true } = {}) {
  const classToggle = vi.fn();
  const examples = {} as HTMLElement;
  const panel = { classList: { toggle: classToggle }, querySelector: () => examples, scrollTop: 456 };
  const showModal = vi.fn();
  const container = { closest: () => panel, scrollIntoView: vi.fn(), querySelector: () => ({ showModal }) } as unknown as HTMLElement;
  const context: NonNullable<Parameters<SpanTailAnalysis["mount"]>[1]> = {
    spanType: { span_type_uid: "request-uid", name: "request" } as SpanTypeStats,
    scope: readScope(new URLSearchParams("api=1&data_dir=demo")),
    view: { startNs: null, endNs: null, selectedUid: "request-uid", bandMinNs: null, bandMaxNs: null, attrFilters: [], maxFiles: null },
    analysis: readTailView(new URLSearchParams()),
    headers: {}, onClearBand: vi.fn(), onBand: vi.fn(),
    onAnalysisChange: (next, clearBand) => {
      context.analysis = next;
      if (clearBand) context.view = { ...context.view, bandMinNs: null, bandMaxNs: null };
      analysis.mount(container, context);
    },
  };
  const analysis = new SpanTailAnalysis();
  const latest = () => vi.mocked(render).mock.calls.filter(c => c[1] === container).at(-1)?.[0];
  if (mount) analysis.mount(container, context);
  return { analysis, context, container, panel, classToggle, latest, showModal };
}

beforeEach(() => {
  vi.mocked(render).mockClear();
  vi.stubGlobal("location", { origin: "http://viewer.example" });
});

afterEach(() => vi.unstubAllGlobals());

describe("automatic selected-span analysis", () => {
  it("starts once with deep-linked bounds and preserves scroll when the report arrives", async () => {
    const pending = deferred<Response>();
    const fetch = vi.fn().mockReturnValue(pending.promise);
    vi.stubGlobal("fetch", fetch);
    const { analysis, context, container, panel, latest, classToggle } = setup({ mount: false });
    analysis.mount(container, {
      ...context,
      view: { ...context.view, bandMinNs: 5e6, bandMaxNs: 20e6, maxFiles: 7 },
    });
    expect(fetch).toHaveBeenCalledOnce();
    const url = fetch.mock.calls[0]![0] as URL;
    expect(url.pathname).toBe("/api/span-tail");
    expect(url.searchParams.get("span_type_uid")).toBe("request-uid");
    expect(url.searchParams.get("min_span_ns")).toBe("5000000");
    expect(url.searchParams.get("max_span_ns")).toBe("20000000");
    expect(url.searchParams.get("max_files")).toBe("7");
    expect(text(latest())).toContain("Computing latency comparison");
    expect(classToggle).toHaveBeenLastCalledWith("tail-analysis-ready", true);
    expect(panel.scrollTop).toBe(0);
    panel.scrollTop = 123;
    pending.resolve(response(report({ boundaryMs: 5 })));
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    expect(panel.scrollTop).toBe(123);
    expect(container.scrollIntoView).not.toHaveBeenCalled();
  });

  it("does not repeat pending or completed requests on equivalent catalog mounts", async () => {
    const pending = deferred<Response>();
    const fetch = vi.fn().mockReturnValue(pending.promise);
    vi.stubGlobal("fetch", fetch);
    const { analysis, context, container, latest } = setup();
    analysis.mount(container, {
      ...context, spanType: { ...context.spanType, count: 200 },
      view: { ...context.view }, headers: { ...context.headers },
    });
    expect(fetch).toHaveBeenCalledOnce();
    expect(fetch.mock.calls[0]![1].signal.aborted).toBe(false);
    pending.resolve(response(report()));
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    analysis.mount(container, { ...context });
    expect(fetch).toHaveBeenCalledOnce();
  });

  it("waits until both a selected span and its container are mounted", async () => {
    const fetch = vi.fn().mockResolvedValue(response(report()));
    vi.stubGlobal("fetch", fetch);
    const { analysis, context, container, latest } = setup({ mount: false });
    analysis.mount(container, null);
    analysis.mount(null, context);
    expect(fetch).not.toHaveBeenCalled();
    analysis.mount(container, context);
    expect(fetch).toHaveBeenCalledOnce();
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
  });

  type Context = ReturnType<typeof setup>["context"];
  const changes: { name: string; change: (context: Context) => Context; parameter: string; expected: string }[] = [
    { name: "span type", change: context => ({
      ...context, spanType: { ...context.spanType, span_type_uid: "different-callsite" },
    }), parameter: "span_type_uid", expected: "different-callsite" },
    { name: "source", change: context => ({
      ...context, scope: { ...context.scope, dataDir: "different-source" },
    }), parameter: "data_dir", expected: "different-source" },
    { name: "time window", change: context => ({
      ...context, view: { ...context.view, startNs: "100", endNs: "200" },
    }), parameter: "start_ns", expected: "100" },
    { name: "attribute filters", change: context => ({
      ...context, view: { ...context.view, attrFilters: [{ key: "status_code", value: "500" }] },
    }), parameter: "attr", expected: "status_code=500" },
    { name: "source file limit", change: context => ({
      ...context, view: { ...context.view, maxFiles: 7 },
    }), parameter: "max_files", expected: "7" },
  ];

  it.each(changes)("automatically starts a fresh comparison after changing $name", async ({ change, parameter, expected }) => {
    const fetch = vi.fn().mockImplementation(() => Promise.resolve(response(report())));
    vi.stubGlobal("fetch", fetch);
    const { analysis, context, container, latest } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    const next = change(context);
    analysis.mount(container, next);
    expect(fetch).toHaveBeenCalledTimes(2);
    expect((fetch.mock.calls[1]![0] as URL).searchParams.get(parameter)).toBe(expected);
    analysis.mount(container, { ...next });
    expect(fetch).toHaveBeenCalledTimes(2);
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
  });

  it("stays stopped after a failure until an explicit retry", async () => {
    const fetch = vi.fn().mockResolvedValueOnce(new Response("Unavailable", { status: 503 }))
      .mockResolvedValueOnce(response(report()));
    vi.stubGlobal("fetch", fetch);
    const { analysis, context, container, latest, classToggle } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("Analysis service unavailable (503)"));
    analysis.mount(container, { ...context });
    analysis.mount(container, context);
    expect(fetch).toHaveBeenCalledOnce();
    expect(classToggle).toHaveBeenLastCalledWith("tail-analysis-ready", false);
    click(latest(), "Analyse source traces")();
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it("stays stopped after cancelling initial loading, including a late response", async () => {
    const pending = deferred<Response>();
    const fetch = vi.fn().mockReturnValueOnce(pending.promise).mockResolvedValueOnce(response(report()));
    vi.stubGlobal("fetch", fetch);
    const { analysis, context, container, latest } = setup();
    click(latest(), "Cancel")();
    expect(fetch.mock.calls[0]![1].signal.aborted).toBe(true);
    analysis.mount(container, { ...context });
    expect(fetch).toHaveBeenCalledOnce();
    pending.resolve(response(report()));
    await pending.promise;
    await Promise.resolve();
    expect(text(latest())).toContain("Analysis cancelled.");
    click(latest(), "Analyse source traces")();
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it("supersedes a pending first report when its histogram band changes", async () => {
    const old = deferred<Response>(), current = deferred<Response>();
    const fetch = vi.fn().mockReturnValueOnce(old.promise).mockReturnValueOnce(current.promise);
    vi.stubGlobal("fetch", fetch);
    const { analysis, context, container, latest } = setup();
    const next = { ...context, view: { ...context.view, bandMinNs: 5e6, bandMaxNs: 20e6 } };
    analysis.mount(container, next);
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(fetch.mock.calls[0]![1].signal.aborted).toBe(true);
    expect((fetch.mock.calls[1]![0] as URL).searchParams.get("min_span_ns")).toBe("5000000");
    expect((fetch.mock.calls[1]![0] as URL).searchParams.get("max_span_ns")).toBe("20000000");
    analysis.mount(container, { ...next });
    expect(fetch).toHaveBeenCalledTimes(2);
    old.resolve(response(report({ signals: [{ ...report().signals[0]!, key: "StaleBand" }] })));
    await old.promise;
    await Promise.resolve();
    expect(text(latest())).not.toContain("StaleBand");
    expect(text(latest())).toContain("Computing latency comparison");
    current.resolve(response(report({ boundaryMs: 5 })));
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
  });
});

describe("Span Explorer investigation", () => {
  it("switches report graphs, file coverage, and limit explanations together", async () => {
    const cases = [
      { key: "first_field", filesUsed: 1, filesMatched: 4, truncated: true, fieldsTruncated: false },
      { key: "second_field", filesUsed: 7, filesMatched: 7, truncated: false, fieldsTruncated: true },
      { key: "complete_field", filesUsed: 9, filesMatched: 9, truncated: false, fieldsTruncated: false },
    ];
    const fetch = vi.fn();
    for (const { key, ...metadata } of cases) {
      const body: TailResponse = { ...metadata, report: report({ signals: [{ ...report().signals[0]!, key }] }) };
      fetch.mockResolvedValueOnce(new Response(JSON.stringify(body)));
    }
    vi.stubGlobal("fetch", fetch);
    const { analysis, context, container, latest } = setup({ mount: false });
    for (const [index, current] of cases.entries()) {
      analysis.mount(container, { ...context, scope: { ...context.scope, dataDir: current.key } });
      await vi.waitFor(() => expect(text(latest())).toContain(current.key));
      expect(text(latest())).toContain(`${current.filesUsed}/${current.filesMatched} files`);
      for (const previous of cases.slice(0, index)) expect(text(latest())).not.toContain(previous.key);
      if (current.filesUsed < current.filesMatched || current.truncated || current.fieldsTruncated) {
        (click(latest(), `Partial sample · ${current.filesUsed}/${current.filesMatched} files`) as (event: unknown) => void)(
          { currentTarget: { focus: vi.fn() } });
        const explanation = text(latest());
        expect(explanation.includes("The one-million-span cap was reached.")).toBe(current.truncated);
        expect(explanation.includes("A numeric or categorical field limit was reached.")).toBe(current.fieldsTruncated);
        expect(explanation).toContain(`${current.filesUsed} of ${current.filesMatched}`);
      } else {
        expect(text(latest())).not.toContain("Partial sample");
        expect(text(latest())).not.toContain("cap was reached");
        expect(text(latest())).not.toContain("field limit was reached");
      }
    }
  });

  it("scopes the whole backend comparison beside its property and keeps reset when the graph disappears", async () => {
    const full = report({ groups: [{ key: "cache_hit", values: ["true", "false"], typical: {}, slow: {} }] });
    const fetch = vi.fn().mockImplementation((url: URL) => Promise.resolve(response(
      url.searchParams.has("group_key") ? report({ histograms: [], groups: [] }) : full)));
    vi.stubGlobal("fetch", fetch);
    const { latest } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("Scope spans by property value"));
    expect(text(latest()).indexOf("Scope spans by property value")).toBeLessThan(text(latest()).indexOf("tail-category-chart"));
    const change = bindings(latest(), "@change=").find(handler => typeof handler === "function"
      && String(handler).includes("scopeTo")) as (event: unknown) => void;
    change({ currentTarget: { value: JSON.stringify(["cache_hit", "false"]) } });
    await vi.waitFor(() => expect(text(latest())).not.toContain("tail-category-chart"));
    expect((fetch.mock.calls[1]![0] as URL).searchParams.get("group_key")).toBe("cache_hit");
    expect((fetch.mock.calls[1]![0] as URL).searchParams.get("group_value")).toBe("false");
    expect(text(latest())).toContain("cache_hit = false");
    click(latest(), "All instances")();
    await vi.waitFor(() => expect(text(latest())).toContain("tail-category-chart"));
    expect((fetch.mock.calls[2]![0] as URL).searchParams.has("group_key")).toBe(false);
  });

  it("keeps the active scope reset usable after a filtered comparison fails", async () => {
    const full = report({ groups: [{ key: "cache_hit", values: ["true", "false"], typical: {}, slow: {} }] });
    const fetch = vi.fn().mockResolvedValueOnce(response(full))
      .mockResolvedValueOnce(new Response("No matching spans", { status: 500 }))
      .mockResolvedValueOnce(response(full));
    vi.stubGlobal("fetch", fetch);
    const { latest } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("Scope spans by property value"));
    const change = bindings(latest(), "@change=").find(handler => typeof handler === "function"
      && String(handler).includes("scopeTo")) as (event: unknown) => void;
    change({ currentTarget: { value: JSON.stringify(["cache_hit", "true"]) } });
    await vi.waitFor(() => expect(text(latest())).toContain("Backend analysis failed (500)"));
    expect(text(latest())).toContain("cache_hit = true");
    click(latest(), "All instances")();
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    expect(fetch).toHaveBeenCalledTimes(3);
    expect((fetch.mock.calls[2]![0] as URL).searchParams.has("group_key")).toBe(false);
  });

  it("opens a tail-value table and keeps whole-range correlation secondary", async () => {
    const field = { ...report().signals[0]!, unit: "ns", correlation: 0, typical: 1e6, slow: 8e6,
      tailDifference: { cdfGap: .8, samplingMargin: .1, lowerBound: .7, comparisons: 10, samplingBudget: .01,
        typicalMedian: 1e6, slowMedian: 8e6, typicalP99: 2e6, slowP99: 12e6 } };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response(report({ signals: [field] }))));
    const { latest, showModal } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("Strong difference in slow spans"));
    expect(text(latest())).not.toContain("Not correlated");
    const handler = bindings(latest(), "@click=").find(fn => typeof fn === "function"
      && String(fn).includes("signalDetails")) as (event: unknown) => void;
    handler({ currentTarget: { focus: vi.fn() } });
    expect(showModal).toHaveBeenCalledOnce();
    expect(text(latest())).toContain("tail-signal-table");
    expect(text(latest())).toContain("Median");
    expect(text(latest())).toContain("p99");
    expect(text(latest())).toContain("12ms");
    expect(text(latest())).toContain("70%");
    expect(text(latest())).toContain("Not correlated");
    expect(text(latest())).toContain("independent observations");
    expect(text(latest())).not.toContain("pairs");
  });

  it("shows properties and numeric results together without a chart tab", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response(report())));
    const { latest, classToggle } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    const rendered = text(latest());
    expect(rendered.indexOf("cache_hit")).toBeLessThan(rendered.indexOf("Numeric fields against elapsed time"));
    expect(rendered).not.toContain("tail-chart-tabs");
    expect(rendered).toContain("Property histogram view");
    expect(rendered).not.toContain("Displayed property");
    expect(rendered).not.toContain("pairs");
    expect(classToggle).toHaveBeenLastCalledWith("tail-analysis-ready", true);
  });

  it("keeps a distinct numeric section when no meaningful properties remain", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response(report({ histograms: [] }))));
    const { latest } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    expect(text(latest())).toContain('aria-label="Numeric fields"');
    expect(text(latest())).not.toContain('tail-property-heading');
  });

  it("keeps elapsed aliases hidden after expanding an older report", async () => {
    const field = report().signals[0]!;
    const signals = [1, 2, 3, 4].map(n => ({ ...field, key: `field_${n}` }));
    signals.splice(1, 0, { ...field, key: "ElapsedAliasOnly", duplicatesLatency: true });
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response(report({ signals }))));
    const { latest } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("field_1"));
    expect(text(latest())).not.toContain("ElapsedAliasOnly");
    expect(text(latest())).toContain("Show all 4 fields");
    click(latest(), "Show all 4 fields")();
    expect(text(latest())).toContain("field_4");
    expect(text(latest())).not.toContain("ElapsedAliasOnly");
  });

  it("shows honest low-count tail evidence and explains coverage separately", async () => {
    const field = { ...report().signals[0]!, correlation: 1, typicalCount: 5, typicalCoverage: .1 };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response(report({ signals: [field] }))));
    const { latest, showModal } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("Too few values to compare"));
    expect(text(latest())).not.toContain("Perfectly correlated");
    expect(text(latest())).not.toContain("Spearman");
    const handler = bindings(latest(), "@click=").find(value => typeof value === "function" && String(value).includes("Limited coverage")) as (event: unknown) => void;
    const trigger = { focus: vi.fn() };
    handler({ currentTarget: trigger });
    expect(showModal).toHaveBeenCalledOnce();
    expect(text(latest())).toContain("Typical has 5 recorded values; at least 20 are required.");
    expect(text(latest())).toContain("coverage warning threshold is 50%.");
    expect(trigger.focus).toHaveBeenCalled();
  });

  it("normalizes Greek-mu microseconds in displayed means", async () => {
    const field = { ...report().signals[0]!, unit: "μs", typical: 2000, slow: 8000 };
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response(report({ signals: [field] }))));
    const { latest } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    expect(text(latest())).toContain("2ms");
    expect(text(latest())).toContain("8ms");
    expect(text(latest())).not.toContain("2,000 μs");
  });

  it("makes previous graphs inert while recomputing and clears them on failure", async () => {
    const pending = deferred<Response>();
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(response(report())).mockReturnValueOnce(pending.promise));
    const { analysis, context, container, latest, classToggle } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    analysis.mount(container, { ...context, view: { ...context.view, bandMinNs: 5e6, bandMaxNs: 20e6 } });
    expect(text(latest())).toContain("Computing latency comparison");
    expect(text(latest())).toContain("recomputing");
    expect(bindings(latest(), "?inert=")).toContain(true);
    pending.resolve(new Response("Synthetic backend error", { status: 500 }));
    await vi.waitFor(() => expect(text(latest())).toContain("Backend analysis failed (500)"));
    expect(text(latest())).not.toContain("work_bytes");
    expect(classToggle).toHaveBeenLastCalledWith("tail-analysis-ready", false);
    expect(() => click(latest(), "Analyse source traces")).not.toThrow();
  });

  it("cancels recomputation and ignores a late response from that request", async () => {
    const pending = deferred<Response>();
    const fetch = vi.fn().mockResolvedValueOnce(response(report())).mockReturnValueOnce(pending.promise);
    vi.stubGlobal("fetch", fetch);
    const { analysis, context, container, latest, classToggle } = setup();
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    analysis.mount(container, { ...context, view: { ...context.view, bandMinNs: 5e6, bandMaxNs: 20e6 } });
    click(latest(), "Cancel")();
    expect(fetch.mock.calls[1]?.[1]?.signal.aborted).toBe(true);
    expect(text(latest())).toContain("Analysis cancelled.");
    expect(text(latest())).not.toContain("work_bytes");
    pending.resolve(response(report()));
    await pending.promise;
    await Promise.resolve();
    expect(text(latest())).toContain("Analysis cancelled.");
    expect(classToggle).toHaveBeenLastCalledWith("tail-analysis-ready", false);
  });

  it("does not cache bytes from a superseded raw-trace request", async () => {
    const pending = deferred<Response>();
    const oldBytes = new Uint8Array([1, 1]).buffer;
    const currentBytes = new Uint8Array([2, 2]).buffer;
    const readOldBytes = vi.fn().mockResolvedValue(oldBytes);
    const fetch = vi.fn().mockImplementation((url: URL, options: RequestInit) => {
      if (url.pathname === "/old.bin") return pending.promise;
      if (url.pathname === "/current.bin") return Promise.resolve(new Response(currentBytes));
      expect(options.body).toEqual(currentBytes);
      return Promise.resolve(response(report()));
    });
    vi.stubGlobal("fetch", fetch);
    const { analysis, context, container, latest } = setup({ mount: false });
    analysis.mount(container, { ...context, scope: { ...context.scope, trace: "/old.bin" } });
    analysis.mount(container, { ...context, scope: { ...context.scope, trace: "/current.bin" } });
    expect(fetch.mock.calls[0]?.[1]?.signal.aborted).toBe(true);
    pending.resolve({ ok: true, arrayBuffer: readOldBytes } as unknown as Response);
    await vi.waitFor(() => expect(readOldBytes).toHaveBeenCalledOnce());
    await vi.waitFor(() => expect(text(latest())).toContain("work_bytes"));
    expect(fetch.mock.calls.map(call => call[0].pathname)).toEqual(["/old.bin", "/current.bin", "/api/span-tail"]);
  });
});
