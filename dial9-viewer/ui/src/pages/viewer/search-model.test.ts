import { describe, it, expect } from "vitest";
import { searchQuery, searchWindow } from "./search-model.js";
import type { SearchIndex } from "./search-model.js";

// The index's per-kind entry shape is internal; the query path reads only
// `hay`/`primary` (ranking) plus the label/sublabel/nav fields (mapping). Build
// those directly so the test stays off a full trace fixture, then cast the
// assembled index.
function task(taskId: number, spawnLoc: string, startNs = 0, endNs = 10) {
  const id = "0x" + taskId.toString(16);
  return {
    taskId, startNs, endNs, label: id, sublabel: spawnLoc,
    primary: id.toLowerCase(), hay: `${id} ${taskId} ${spawnLoc}`.toLowerCase(),
  };
}
function span(spanId: string, name: string, fields = "") {
  return {
    spanId, startNs: 0, endNs: 5, label: name, sublabel: fields,
    primary: name.toLowerCase(), hay: `${name} ${fields}`.toLowerCase(),
  };
}
function poi(type: string, worker: number) {
  return {
    poi: { type, worker, time: 0, value: 0 }, label: type, sublabel: `W${worker}`,
    primary: type.toLowerCase(), hay: `${type} w${worker}`.toLowerCase(),
  };
}
function index(over: {
  tasks?: unknown[]; spans?: unknown[]; pois?: unknown[];
}): SearchIndex {
  return { tasks: over.tasks ?? [], spans: over.spans ?? [], pois: over.pois ?? [] } as unknown as SearchIndex;
}

describe("searchQuery", () => {
  it("returns nothing for an empty / whitespace query", () => {
    const idx = index({ tasks: [task(0x1a2b, "server.rs:42")] });
    expect(searchQuery(idx, "").total).toBe(0);
    expect(searchQuery(idx, "   ").total).toBe(0);
  });

  it("matches a task by hex id (with or without 0x) and by spawn location", () => {
    const idx = index({ tasks: [task(0x1a2b, "server.rs:42")] });
    expect(searchQuery(idx, "0x1a2b").tasks).toHaveLength(1);
    expect(searchQuery(idx, "1a2b").tasks).toHaveLength(1);
    expect(searchQuery(idx, "server.rs").tasks[0]!.nav).toEqual({
      kind: "task", taskId: 0x1a2b, startNs: 0, endNs: 10,
    });
  });

  it("matches a span by name and by field text", () => {
    const idx = index({ spans: [span("s1", "http.request", "method=GET")] });
    expect(searchQuery(idx, "http").spans).toHaveLength(1);
    expect(searchQuery(idx, "get").spans[0]!.nav).toEqual({
      kind: "span", spanId: "s1", startNs: 0, endNs: 5,
    });
  });

  it("matches a POI by type and by worker token", () => {
    const idx = index({ pois: [poi("sched", 1)] });
    expect(searchQuery(idx, "sched").pois).toHaveLength(1);
    expect(searchQuery(idx, "w1").pois).toHaveLength(1);
    expect(searchQuery(idx, "sched").pois[0]!.nav.kind).toBe("poi");
  });

  it("ranks a prefix hit above a body-only hit", () => {
    const idx = index({ spans: [span("a", "unpoll"), span("b", "poll")] });
    // "poll" is a prefix of span b's name, only a substring of "unpoll".
    expect(searchQuery(idx, "poll").spans.map((r) => r.label)).toEqual(["poll", "unpoll"]);
  });

  it("caps each group at perKind", () => {
    const tasks = Array.from({ length: 10 }, (_, i) => task(0x100 + i, "loc"));
    const res = searchQuery(index({ tasks }), "loc", 3);
    expect(res.tasks).toHaveLength(3);
    expect(res.total).toBe(3);
  });

  it("groups across kinds and totals them", () => {
    const idx = index({
      tasks: [task(0xa, "poll-loc")],
      spans: [span("s", "poll")],
      pois: [poi("long-poll", 2)],
    });
    const res = searchQuery(idx, "poll");
    expect([res.tasks.length, res.spans.length, res.pois.length]).toEqual([1, 1, 1]);
    expect(res.total).toBe(3);
  });
});

describe("searchWindow", () => {
  it("pads a wide range by 30% each side", () => {
    // dur 1e7 -> 30% = 3e6, which beats the 0.5ms floor.
    const w = searchWindow(10e6, 20e6, 0, 1e9);
    expect(w).toEqual({ start: 7e6, end: 23e6 });
  });

  it("floors the pad at 0.5ms for a narrow range", () => {
    const w = searchWindow(1e6, 1e6 + 100, 0, 1e9);
    expect(w.start).toBe(1e6 - 5e5); // 0.5ms floor, not 30% of 100
    expect(w.end).toBe(1e6 + 100 + 5e5);
  });

  it("clamps to minTs / maxTs", () => {
    const w = searchWindow(100, 200, 150, 180);
    expect(w).toEqual({ start: 150, end: 180 });
  });

  it("keeps start <= end even on a point-sized extent", () => {
    const w = searchWindow(5000, 5000, 8000, 8000);
    expect(w.start).toBeLessThanOrEqual(w.end);
  });
});
