// Tests for the spans track's pure model - the behavioral checks that do NOT
// need a browser: the filter semantics, the render-set / focus repositioning,
// the binary-searched visibility window, the selection-driven dimming input,
// and the unmatched-span surfacing. Span records are built synthetically; one
// case drives buildSpanData through real SpanEnter/SpanExit events.

import { describe, it, expect } from "vitest";
import type { CustomTraceEvent, TracingSpan, WorkerLane } from "../../lib/trace/index.js";
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
import { ColumnarSpansBuilder } from "../../lib/trace/columnar-spans.js";

// ── Synthetic span builders ──────────────────────────────────────────────

interface SpanOpts {
  parent?: string | null;
  worker?: number;
  fields?: Record<string, unknown>;
  segments?: { start: number; end: number; workerId: number }[];
  activeNs?: number;
  taskId?: number;
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
    taskId: o.taskId ?? null,
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
function exit(ts: number, spanId: string, name: string, worker = 0): CustomTraceEvent {
  return {
    name: "SpanExit:demo",
    timestamp: ts,
    fields: { worker_id: worker, span_id: spanId, span_name: name } as CustomTraceEvent["fields"],
    units: null,
  };
}
function lane(polls: { start: number; end: number; taskId: number }[]): WorkerLane {
  return {
    polls: polls.map((p) => ({ ...p, spawnLocId: null, spawnLoc: null })),
    parks: [],
    actives: [],
    cpuSampleTimes: [],
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

  it("surfaces enter-without-exit as unmatchedSpans", () => {
    const data = computeSpanTrackData([
      enter(100, "a", "load"),
      exit(300, "a", "load"),
      enter(150, "leak", "orphan"), // no exit -> unmatched, never dropped
    ]);
    expect(data.allSpans.map((s) => s.spanId)).toEqual(["a"]);
    expect(data.unmatchedSpans.map((u) => u.spanId)).toEqual(["leak"]);
  });

  it("reconstructs active/idle segments and resolves taskId from workerSpans", () => {
    // One on-wire span [1000, 9000] entered on worker 0, exited on worker 1.
    // Task 42 polled three times in that window; reconstruction replaces the
    // coarse segment with the real on-CPU polls so idle gaps materialize.
    const span = computeSpanTrackData(
      [enter(1000, "req", "GET /jobs"), exit(9000, "req", "GET /jobs", 1)],
      {
        0: lane([
          { start: 900, end: 1100, taskId: 42 },
          { start: 8800, end: 8900, taskId: 42 },
        ]),
        1: lane([{ start: 4000, end: 4100, taskId: 42 }]),
      },
    ).allSpans[0]!;
    expect(span.taskId).toBe(42);
    expect(span.segments.length).toBe(3);
    expect(span.activeNs).toBe(300); // 3x100 on-CPU, not the 8000 wall-clock window
  });

  it("keeps the coarse on-wire segment and null taskId without workerSpans", () => {
    const span = computeSpanTrackData([
      enter(1000, "req", "GET /jobs"),
      exit(9000, "req", "GET /jobs"),
    ]).allSpans[0]!;
    expect(span.taskId).toBeNull();
    expect(span.activeNs).toBe(8000);
  });
});

// ── Filtering ─────────────────────────────────────────────────────────────

describe("spanMatchesFilter", () => {
  const durs = new Map<string, number[]>([["req", [10, 20, 30, 40, 100]]]);

  it("matches on span name substring, case-insensitive", () => {
    const s = span("1", "HttpRequest", 0, 10);
    expect(spanMatchesFilter(s, { ...noFilter, text: "request" }, durs)).toBe(true);
    expect(spanMatchesFilter(s, { ...noFilter, text: "grpc" }, durs)).toBe(false);
  });

  it("matches on field key or value", () => {
    const s = span("1", "req", 0, 10, { fields: { route: "/users" } });
    expect(spanMatchesFilter(s, { ...noFilter, text: "route" }, durs)).toBe(true);
    expect(spanMatchesFilter(s, { ...noFilter, text: "/users" }, durs)).toBe(true);
    expect(spanMatchesFilter(s, { ...noFilter, text: "/posts" }, durs)).toBe(false);
  });

  it("applies the percentile floor of the name's duration distribution", () => {
    // P50 index = floor(5 * 50/100) = 2 -> threshold durs[2] = 30.
    const below = span("1", "req", 0, 20); // dur 20 < 30
    const above = span("2", "req", 0, 50); // dur 50 >= 30
    expect(spanMatchesFilter(below, { ...noFilter, pctFloor: 50 }, durs)).toBe(false);
    expect(spanMatchesFilter(above, { ...noFilter, pctFloor: 50 }, durs)).toBe(true);
  });

  it("AND-combines name chips with the text + percentile filters", () => {
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
  it("ranks a span within its name's duration distribution", () => {
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

// ── filterVisibleSpans: columnar path parity + bounded window ─────────────

/** A SpanTrackData whose spans live in a ColumnarSpans store (allSpans empty),
 *  mirroring durationsByName so the filter works identically to the fat path. */
function columnarTrackData(spans: TracingSpan[]): SpanTrackData {
  const b = new ColumnarSpansBuilder();
  const parentBySpanId = new Map<string, string | null>();
  const durationsByName = new Map<string, number[]>();
  for (const s of spans) {
    parentBySpanId.set(s.spanId, s.parentSpanId);
    b.push(s.start, s.end, s.spanId, s.spanName, s.parentSpanId, s.taskId, s.activeNs, s.segments, s.fields);
    const durs = durationsByName.get(s.spanName) ?? [];
    durs.push(s.end - s.start);
    durationsByName.set(s.spanName, durs);
  }
  for (const d of durationsByName.values()) d.sort((a, b2) => a - b2);
  const { store } = b.finish(parentBySpanId);
  return {
    allSpans: [],
    columnarSpans: store,
    spanMeta: new Map(),
    childrenByParent: new Map(),
    unmatchedSpans: [],
    maxDepth: 0,
    spanNames: [...new Set(spans.map((s) => s.spanName))].sort(),
    durationsByName,
  };
}

describe("filterVisibleSpans (columnar path)", () => {
  // Nested + long-straddler mix: end is NOT monotonic in start order.
  const spans = [
    span("a", "s", 0, 5),
    span("straddler", "s", 1, 5000),
    span("b", "s", 100, 120),
    span("c", "s", 4000, 6000),
    span("d", "s", 4010, 4011),
    span("e", "s", 9000, 9100),
  ];
  const fat = trackData(spans);
  const col = columnarTrackData(spans);

  it("matches the fat path across every window (incl. panned far right)", () => {
    for (const [vs, ve] of [[0, 50], [110, 130], [4005, 4020], [5500, 8000], [9000, 9100], [20000, 21000]] as const) {
      const got = filterVisibleSpans(col, vs, ve, noFilter).map((s) => s.spanId).sort();
      const want = filterVisibleSpans(fat, vs, ve, noFilter).map((s) => s.spanId).sort();
      expect(got, `window ${vs}..${ve}`).toEqual(want);
    }
  });

  it("keeps the long straddler when the window is deep inside it", () => {
    const vis = filterVisibleSpans(col, 3000, 3001, noFilter).map((s) => s.spanId);
    expect(vis).toContain("straddler");
  });

  it("window bound skips rows left of viewStart - maxSpanDur", () => {
    // Panned to the far-right span; the window must not start at row 0.
    const { lo } = col.columnarSpans!.windowRows(9000, 9100);
    expect(lo).toBeGreaterThan(0);
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

  it("focused view includes descendants and pins the focus at the top", () => {
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
  it("walks the ancestor chain from a clicked span", () => {
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

describe("spanHighlight (dim input)", () => {
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
  it("emits one keyed chip per name with active + color", () => {
    const data = trackData([span("1", "auth", 0, 10), span("2", "load", 0, 10)]);
    const chips = spanChipModels(data, new Set(["auth"]), (n) => `c-${n}`);
    expect(chips.map((c) => [c.name, c.active])).toEqual([
      ["auth", true],
      ["load", false],
    ]);
    expect(chips[0]?.color).toBe("c-auth");
  });
});

describe("spanLabelModel + focusInfoLine", () => {
  it("is null when nothing is focused (label shows 'Spans')", () => {
    const data = trackData([span("1", "auth", 0, 10)]);
    expect(spanLabelModel(null, data)).toBeNull();
  });

  it("renders the focused span name + a copyable row per field", () => {
    const data = trackData([span("1", "auth", 0, 10, { fields: { user: "abc" } })]);
    const label = spanLabelModel("1", data);
    expect(label?.name).toBe("auth");
    expect(label?.rows).toEqual([{ key: "user", display: "abc", copy: "abc" }]);
  });

  it("formats the focus readout name: dur (P% of N) P50 P99", () => {
    const durs = new Map<string, number[]>([["req", [100, 200, 300]]]);
    const line = focusInfoLine(span("1", "req", 0, 200), durs);
    expect(line).toMatch(/^req: .* \(P\d+ of 3\) · P50=.* P99=.*$/);
  });
});
