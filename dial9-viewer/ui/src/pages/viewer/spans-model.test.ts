// Tests for the spans track's pure model - the behavioral checks that do NOT
// need a browser: the filter semantics, the render-set / focus repositioning,
// the binary-searched visibility window, the selection-driven dimming input,
// and the unmatched-span surfacing. Span records are built synthetically; one
// case drives buildSpanData through real SpanEnter/SpanExit events.

import { describe, it, expect } from "vitest";
import type { CustomTraceEvent, TracingSpan } from "../../lib/trace/index.js";
import {
  buildSpanRenderModel,
  computeSpanTrackData,
  filterVisibleSpans,
  focusInfoLine,
  isFilterActive,
  spanChipModels,
  spanFocusChain,
  spanHighlight,
  spanLabelModel,
  spanMatchesFilter,
  spanPercentileRank,
  upperBoundByStart,
  type SpanFilterState,
  type SpanTrackData,
} from "./spans-model.js";

// ── Synthetic span builders ──────────────────────────────────────────────

interface SpanOpts {
  parent?: string | null;
  worker?: number;
  fields?: Record<string, unknown>;
  segments?: { start: number; end: number; workerId: number }[];
  activeNs?: number;
}

function span(id: string, name: string, start: number, end: number, o: SpanOpts = {}): TracingSpan {
  const segments = o.segments ?? [{ start, end, workerId: o.worker ?? 0 }];
  const activeNs = o.activeNs ?? segments.reduce((n, s) => n + (s.end - s.start), 0);
  return {
    start,
    end,
    spanId: id,
    spanName: name,
    fields: (o.fields ?? {}) as TracingSpan["fields"],
    parentSpanId: o.parent ?? null,
    segments,
    activeNs,
    depth: 0,
  };
}

/** Build a full SpanTrackData from span records (bypassing buildSpanData). */
function trackData(spans: TracingSpan[]): SpanTrackData {
  const childrenByParent = new Map<string | null, string[]>();
  const spanMeta: SpanTrackData["spanMeta"] = new Map();
  const durationsByName = new Map<string, number[]>();
  const names = new Set<string>();
  for (const s of spans) {
    names.add(s.spanName);
    const durs = durationsByName.get(s.spanName) ?? [];
    durs.push(s.end - s.start);
    durationsByName.set(s.spanName, durs);
    spanMeta.set(s.spanId, {
      spanName: s.spanName,
      fields: s.fields,
      parentSpanId: s.parentSpanId,
    });
    const kids = childrenByParent.get(s.parentSpanId) ?? [];
    kids.push(s.spanId);
    childrenByParent.set(s.parentSpanId, kids);
  }
  for (const d of durationsByName.values()) d.sort((a, b) => a - b);
  return {
    allSpans: [...spans].sort((a, b) => a.start - b.start),
    spanMeta,
    childrenByParent,
    unmatchedSpans: [],
    maxDepth: 0,
    spanNames: [...names].sort(),
    durationsByName,
  };
}

const noFilter: SpanFilterState = { text: "", pctFloor: 0, selectedNames: new Set() };

// ── computeSpanTrackData (buildSpanData path) ─────────────────────────────

function enter(ts: number, spanId: string, name: string, extra: Record<string, unknown> = {}): CustomTraceEvent {
  return {
    name: "SpanEnter:demo",
    timestamp: ts,
    fields: { worker_id: 0, span_id: spanId, span_name: name, ...extra } as CustomTraceEvent["fields"],
    units: null,
  };
}
function exit(ts: number, spanId: string, name: string): CustomTraceEvent {
  return {
    name: "SpanExit:demo",
    timestamp: ts,
    fields: { worker_id: 0, span_id: spanId, span_name: name } as CustomTraceEvent["fields"],
    units: null,
  };
}

describe("computeSpanTrackData", () => {
  it("returns the empty resting data for a null / empty trace", () => {
    expect(computeSpanTrackData(null).allSpans).toEqual([]);
    expect(computeSpanTrackData([]).spanNames).toEqual([]);
  });

  it("derives spans, sorted names, and per-name durations from span events", () => {
    const data = computeSpanTrackData([
      enter(100, "a", "load", { key: "x" }),
      exit(300, "a", "load"),
      enter(150, "b", "auth"),
      exit(400, "b", "auth"),
    ]);
    expect(data.allSpans.map((s) => s.spanId)).toEqual(["a", "b"]);
    expect(data.spanNames).toEqual(["auth", "load"]); // sorted
    expect(data.durationsByName.get("load")).toEqual([200]);
    expect(data.durationsByName.get("auth")).toEqual([250]);
  });

  it("surfaces enter-without-exit as unmatchedSpans (J13; T17 notes 6+7)", () => {
    const data = computeSpanTrackData([
      enter(100, "a", "load"),
      exit(300, "a", "load"),
      enter(150, "leak", "orphan"), // no exit -> unmatched, never dropped
    ]);
    expect(data.allSpans.map((s) => s.spanId)).toEqual(["a"]);
    expect(data.unmatchedSpans.map((u) => u.spanId)).toEqual(["leak"]);
  });
});

// ── Filtering ─────────────────────────────────────────────────────────────

describe("spanMatchesFilter", () => {
  const durs = new Map<string, number[]>([["req", [10, 20, 30, 40, 100]]]);

  it("matches on span name substring, case-insensitive (J7)", () => {
    const s = span("1", "HttpRequest", 0, 10);
    expect(spanMatchesFilter(s, { ...noFilter, text: "request" }, durs)).toBe(true);
    expect(spanMatchesFilter(s, { ...noFilter, text: "grpc" }, durs)).toBe(false);
  });

  it("matches on field key or value (J7)", () => {
    const s = span("1", "req", 0, 10, { fields: { route: "/users" } });
    expect(spanMatchesFilter(s, { ...noFilter, text: "route" }, durs)).toBe(true);
    expect(spanMatchesFilter(s, { ...noFilter, text: "/users" }, durs)).toBe(true);
    expect(spanMatchesFilter(s, { ...noFilter, text: "/posts" }, durs)).toBe(false);
  });

  it("applies the percentile floor of the name's duration distribution (J8)", () => {
    // P50 index = floor(5 * 50/100) = 2 -> threshold durs[2] = 30.
    const below = span("1", "req", 0, 20); // dur 20 < 30
    const above = span("2", "req", 0, 50); // dur 50 >= 30
    expect(spanMatchesFilter(below, { ...noFilter, pctFloor: 50 }, durs)).toBe(false);
    expect(spanMatchesFilter(above, { ...noFilter, pctFloor: 50 }, durs)).toBe(true);
  });

  it("AND-combines name chips with the text + percentile filters (J9)", () => {
    const s = span("1", "req", 0, 50);
    const names = new Set(["other"]);
    expect(spanMatchesFilter(s, { ...noFilter, selectedNames: names }, durs)).toBe(false);
    const named = new Set(["req"]);
    expect(spanMatchesFilter(s, { text: "req", pctFloor: 50, selectedNames: named }, durs)).toBe(true);
  });

  it("isFilterActive reflects any active dimension", () => {
    expect(isFilterActive(noFilter)).toBe(false);
    expect(isFilterActive({ ...noFilter, text: "x" })).toBe(true);
    expect(isFilterActive({ ...noFilter, pctFloor: 90 })).toBe(true);
    expect(isFilterActive({ ...noFilter, selectedNames: new Set(["a"]) })).toBe(true);
  });
});

describe("spanPercentileRank", () => {
  it("ranks a span within its name's duration distribution (J5/J6)", () => {
    const durs = new Map<string, number[]>([["req", [10, 20, 30, 40]]]);
    expect(spanPercentileRank(span("1", "req", 0, 40), durs)).toBe(100);
    expect(spanPercentileRank(span("2", "req", 0, 20), durs)).toBe(50);
    expect(spanPercentileRank(span("3", "gone", 0, 5), durs)).toBeNull();
  });
});

// ── Visibility window (binary search) ─────────────────────────────────────

describe("visibility window", () => {
  const spans = [span("a", "s", 0, 100), span("b", "s", 200, 300), span("c", "s", 500, 600)];
  const data = trackData(spans);

  it("upperBoundByStart binary-bounds the right edge", () => {
    expect(upperBoundByStart(data.allSpans, 250)).toBe(2); // a,b start <= 250
    expect(upperBoundByStart(data.allSpans, -1)).toBe(0);
    expect(upperBoundByStart(data.allSpans, 10_000)).toBe(3);
  });

  it("keeps spans overlapping the window and drops the rest", () => {
    const vis = filterVisibleSpans(data, 150, 350, noFilter);
    expect(vis.map((s) => s.spanId)).toEqual(["b"]);
  });

  it("keeps a long span whose start precedes the window (end still inside)", () => {
    const long = trackData([span("long", "s", 0, 1000), span("late", "s", 2000, 2100)]);
    const vis = filterVisibleSpans(long, 500, 600, noFilter);
    expect(vis.map((s) => s.spanId)).toEqual(["long"]);
  });

  it("applies the filter within the window", () => {
    const named = trackData([span("a", "keep", 0, 100), span("b", "drop", 10, 90)]);
    const vis = filterVisibleSpans(named, 0, 200, { ...noFilter, text: "keep" });
    expect(vis.map((s) => s.spanId)).toEqual(["a"]);
  });
});

// ── Render model ──────────────────────────────────────────────────────────

describe("buildSpanRenderModel", () => {
  const parent = span("p", "parent", 0, 1000, { activeNs: 1000 });
  const child = span("c", "child", 100, 400, { parent: "p", activeNs: 300 });
  const data = trackData([parent, child]);
  const base = { data, viewStart: 0, viewEnd: 1000, drawW: 400, canvasH: 120, filter: noFilter };

  it("root view renders roots-only with an 'N spans · M clusters' readout", () => {
    const m = buildSpanRenderModel({ ...base, focusedSpanId: null });
    // selectSpanRenderSet roots view => only the parent (child has a parent).
    expect(m.renderCount).toBe(1);
    expect(m.info).toMatch(/^1 spans · \d+ clusters$/);
    expect(m.emptyReason).toBeNull();
  });

  it("focused view includes descendants and pins the focus at the top (J2)", () => {
    const m = buildSpanRenderModel({ ...base, focusedSpanId: "p" });
    expect(m.renderCount).toBe(2); // parent + child
    const focus = m.buckets.find((b) => b.representative.spanId === "p");
    const other = m.buckets.find((b) => b.representative.spanId === "c");
    expect(focus?.y).toBe(4); // PARENT_Y
    expect(other?.y).toBe(34); // CHILD_Y
    expect(m.info).toContain("parent:"); // rich focus readout
  });

  it("reports no-visible when nothing overlaps the window", () => {
    const m = buildSpanRenderModel({ ...base, viewStart: 5000, viewEnd: 6000, focusedSpanId: null });
    expect(m.emptyReason).toBe("no-visible");
    expect(m.buckets).toEqual([]);
  });

  it("reports no-visible on a degenerate (zero-width) draw area", () => {
    const m = buildSpanRenderModel({ ...base, drawW: 0, focusedSpanId: null });
    expect(m.emptyReason).toBe("no-visible");
  });
});

// ── Focus chain + dimming ─────────────────────────────────────────────────

describe("spanFocusChain", () => {
  it("walks the ancestor chain from a clicked span (J2)", () => {
    const data = trackData([
      span("root", "r", 0, 1000),
      span("mid", "m", 10, 900, { parent: "root" }),
      span("leaf", "l", 20, 100, { parent: "mid" }),
    ]);
    expect([...spanFocusChain("leaf", data)].sort()).toEqual(["leaf", "mid", "root"]);
  });

  it("terminates on a parent cycle", () => {
    const data = trackData([span("a", "a", 0, 10, { parent: "b" }), span("b", "b", 0, 10, { parent: "a" })]);
    const chain = spanFocusChain("a", data);
    expect(chain.has("a")).toBe(true);
    expect(chain.has("b")).toBe(true);
  });
});

describe("spanHighlight (S4 dim input)", () => {
  it("is inactive with no span focus (no dimming)", () => {
    const h = spanHighlight({ spanFocus: null });
    expect(h.active).toBe(false);
    expect(h.set.size).toBe(0);
  });

  it("activates and exposes the chain when a span is focused", () => {
    const chain = new Set(["leaf", "mid"]);
    const h = spanHighlight({ spanFocus: { spanId: "leaf", chain } });
    expect(h.active).toBe(true);
    expect([...h.set].sort()).toEqual(["leaf", "mid"]);
  });
});

// ── Chip + label models ───────────────────────────────────────────────────

describe("spanChipModels", () => {
  it("emits one keyed chip per name with active + color (J9/F7)", () => {
    const data = trackData([span("1", "auth", 0, 10), span("2", "load", 0, 10)]);
    const chips = spanChipModels(data, new Set(["auth"]), (n) => `c-${n}`);
    expect(chips.map((c) => [c.name, c.active])).toEqual([
      ["auth", true],
      ["load", false],
    ]);
    expect(chips[0]?.color).toBe("c-auth");
  });
});

describe("spanLabelModel + focusInfoLine (J3/J5)", () => {
  it("is null when nothing is focused (label shows 'Spans')", () => {
    const data = trackData([span("1", "auth", 0, 10)]);
    expect(spanLabelModel(null, data)).toBeNull();
  });

  it("renders the focused span name + a copyable row per field (J3/J4)", () => {
    const data = trackData([span("1", "auth", 0, 10, { fields: { user: "abc" } })]);
    const label = spanLabelModel("1", data);
    expect(label?.name).toBe("auth");
    expect(label?.rows).toEqual([{ key: "user", display: "abc", copy: "abc" }]);
  });

  it("formats the focus readout name: dur (P% of N) P50 P99 (J5)", () => {
    const durs = new Map<string, number[]>([["req", [100, 200, 300]]]);
    const line = focusInfoLine(span("1", "req", 0, 200), durs);
    expect(line).toMatch(/^req: .* \(P\d+ of 3\) · P50=.* P99=.*$/);
  });
});
