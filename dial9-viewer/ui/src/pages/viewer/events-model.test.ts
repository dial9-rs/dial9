// Pure-model tests for the custom-events track. The store/DOM-boundary checks
// (click-to-pin dispatch, hover dispatch, dim-on-selection render input) live
// in events-track.test.ts; the browser-driven halves (row-walker, visual) are
// listed in HANDOFF.md.
//
// Coverage here:
//   - bounded-scan: filterVisibleEvents binary-searches BOTH window edges, so
//     the per-frame work is O(log N + window), never the legacy O(all generic
//     events) scan. Proven with an access-counting Proxy.
//   - clustering + geometry, name filter, info counts, task/poll resolution,
//     and the highlight-task derivation.

import { describe, it, expect } from "vitest";
import {
  buildEventRenderModel,
  computeEventTrackData,
  eventHighlightTask,
  eventMatchesFilter,
  filterVisibleEvents,
  lowerBoundByTimestamp,
  resolveClusterTask,
  resolvePollForEvent,
  resolveTaskForEvent,
  upperBoundByTimestamp,
  type EventTrackData,
} from "./events-model.js";
import type {
  CustomTraceEvent,
  PollSpan,
  WorkerLane,
} from "../../lib/trace/index.js";

function ev(
  name: string,
  timestamp: number,
  fields: Record<string, unknown> = {},
): CustomTraceEvent {
  return {
    name,
    timestamp,
    fields: fields as CustomTraceEvent["fields"],
    units: null,
  };
}

// ── Generic-event extraction ─────────────────────────────────────────────

describe("computeEventTrackData", () => {
  it("drops span lifecycle events, sorts by ts, and lists unique names", () => {
    const data = computeEventTrackData([
      ev("beta", 30),
      ev("SpanEnter:foo::bar:f:1", 5), // prefix -> excluded
      ev("alpha", 10),
      ev("SpanExitEvent", 6), // exact -> excluded
      ev("alpha", 20),
      ev("SpanCloseEvent", 7), // exact -> excluded
      ev("SpanEnter__Derived", 8), // derived prefix -> excluded
      ev("SpanClose__Derived", 9), // derived prefix -> excluded
      {
        ...ev("producer:RequestMetrics", 10),
        singleEventSpan: {
          start: 1,
          end: 10,
          name: "request",
          spanType: "producer",
          threadId: null,
          taskId: null,
          workerId: null,
          fields: {},
          units: null,
        },
      },
    ]);
    expect(data.events.map((e) => e.timestamp)).toEqual([10, 20, 30]);
    expect(data.events.map((e) => e.name)).toEqual(["alpha", "alpha", "beta"]);
    expect(data.eventNames).toEqual(["alpha", "beta"]);
  });

  it("returns the shared empty data for null / span-only input", () => {
    expect(computeEventTrackData(null).events).toHaveLength(0);
    expect(computeEventTrackData([ev("SpanEnter:x", 1)]).events).toHaveLength(0);
  });

  it("keeps unannotated metrique-named events", () => {
    expect(computeEventTrackData([ev("metrique:RequestMetrics", 10)]).events)
      .toHaveLength(1);
  });
});

// ── Binary-search window: bounded scan ───────────────────────────────────

describe("visibility window binary search", () => {
  const events = Array.from({ length: 20 }, (_, i) => ev("e", i * 10));
  const data: EventTrackData = { events, eventNames: ["e"] };

  it("lowerBound / upperBound bracket the inclusive window", () => {
    // window [50, 120]: first ts>=50 is index 5 (ts 50); first ts>120 is
    // index 13 (ts 130).
    expect(lowerBoundByTimestamp(events, 50)).toBe(5);
    expect(upperBoundByTimestamp(events, 120)).toBe(13);
    // exact-edge inclusivity: ts 120 is index 12, included by upperBound(120).
    const vis = filterVisibleEvents(data, 50, 120, new Set());
    expect(vis.map((e) => e.timestamp)).toEqual([50, 60, 70, 80, 90, 100, 110, 120]);
  });

  it("scans O(log N + window), NOT O(N) (bounded scan)", () => {
    const N = 50_000;
    const big = Array.from({ length: N }, (_, i) => ev("e", i));
    let reads = 0;
    const proxy = new Proxy(big, {
      get(target, prop, recv) {
        if (typeof prop === "string" && /^[0-9]+$/.test(prop)) reads++;
        return Reflect.get(target, prop, recv);
      },
    });
    const windowed: EventTrackData = { events: proxy, eventNames: ["e"] };
    // A narrow window near the middle: indices [25000, 25009].
    const vis = filterVisibleEvents(windowed, 25_000, 25_009, new Set());
    expect(vis.map((e) => e.timestamp)).toEqual([
      25_000, 25_001, 25_002, 25_003, 25_004, 25_005, 25_006, 25_007, 25_008,
      25_009,
    ]);
    // Two binary searches (~2*log2(50000) ≈ 32 reads) plus a 10-event window.
    // A linear scan would touch all 50000 - assert we are nowhere near it.
    expect(reads).toBeLessThan(100);
    expect(reads).toBeLessThan(N);
  });
});

// ── Name filter ──────────────────────────────────────────────────────────

describe("eventMatchesFilter", () => {
  it("passes everything when no names are selected", () => {
    expect(eventMatchesFilter(ev("x", 1), new Set())).toBe(true);
  });
  it("keeps only selected names", () => {
    const sel = new Set(["keep"]);
    expect(eventMatchesFilter(ev("keep", 1), sel)).toBe(true);
    expect(eventMatchesFilter(ev("drop", 1), sel)).toBe(false);
  });
  it("filterVisibleEvents applies the name filter within the window", () => {
    const data: EventTrackData = {
      events: [ev("a", 10), ev("b", 20), ev("a", 30)],
      eventNames: ["a", "b"],
    };
    const vis = filterVisibleEvents(data, 0, 100, new Set(["a"]));
    expect(vis.map((e) => e.timestamp)).toEqual([10, 30]);
  });
});

// ── Clustering + tick geometry + info ────────────────────────────────────

describe("buildEventRenderModel", () => {
  const colorOf = (n: string): string => (n === "a" ? "#aaa" : "#bbb");
  const noTask = (): number | null => null;

  it("clusters same-pixel events, sizes ticks, and stripes mixed names", () => {
    // viewStart=0 viewEnd=100 drawW=100 -> ns maps 1:1 to px.
    const data: EventTrackData = {
      events: [ev("a", 10), ev("b", 10), ev("a", 50)],
      eventNames: ["a", "b"],
    };
    const model = buildEventRenderModel({
      data,
      viewStart: 0,
      viewEnd: 100,
      drawW: 100,
      canvasH: 40,
      selectedNames: new Set(),
      taskOf: noTask,
      colorOf,
    });
    expect(model.emptyReason).toBeNull();
    expect(model.info).toBe("3 events · 2 markers");
    expect(model.buckets).toHaveLength(2);

    const c10 = model.buckets.find((b) => b.px === 10)!;
    expect(c10.events).toHaveLength(2);
    // w = max(3, min(3 + log2(2)*2, 10)) = 5; baseAlpha = min(0.4+2*.15,1)=0.7.
    expect(c10.w).toBe(5);
    expect(c10.baseAlpha).toBeCloseTo(0.7, 10);
    expect(c10.color).toBe("#aaa"); // rep = first event "a"
    expect(c10.secondColor).toBe("#bbb"); // mixed -> "b" stripe
    expect(c10.h).toBe(36); // canvasH - 2*pad
    expect(c10.hitW).toBe(12); // padded from w=5
    expect(c10.hitX).toBe(4); // px 10 - hitW/2

    const c50 = model.buckets.find((b) => b.px === 50)!;
    expect(c50.events).toHaveLength(1);
    expect(c50.w).toBe(3);
    expect(c50.baseAlpha).toBeCloseTo(0.55, 10);
    expect(c50.secondColor).toBeNull();
  });

  it("reports the no-events vs no-visible resting reasons", () => {
    const empty = buildEventRenderModel({
      data: { events: [], eventNames: [] },
      viewStart: 0,
      viewEnd: 100,
      drawW: 100,
      canvasH: 40,
      selectedNames: new Set(),
      taskOf: noTask,
      colorOf,
    });
    expect(empty.emptyReason).toBe("no-events");

    const panned = buildEventRenderModel({
      data: { events: [ev("a", 500)], eventNames: ["a"] },
      viewStart: 0,
      viewEnd: 100,
      drawW: 100,
      canvasH: 40,
      selectedNames: new Set(),
      taskOf: noTask,
      colorOf,
    });
    expect(panned.emptyReason).toBe("no-visible");
  });
});

// ── Task / poll resolution ───────────────────────────────────────────────

function poll(start: number, end: number, taskId: number): PollSpan {
  return { start, end, taskId, spawnLocId: null, spawnLoc: null };
}
function lane(polls: PollSpan[]): WorkerLane {
  return { polls, parks: [], actives: [], cpuSampleTimes: [] };
}

describe("resolveTaskForEvent", () => {
  const lanes: Record<number, WorkerLane> = {
    0: lane([poll(0, 100, 42)]),
    1: lane([poll(0, 100, 99)]),
  };
  const workerIds = [0, 1];

  it("prefers the task_id field", () => {
    expect(resolveTaskForEvent(ev("e", 50, { task_id: 7 }), lanes, workerIds)).toBe(7);
  });
  it("resolves via worker_id + the poll at the timestamp", () => {
    expect(resolveTaskForEvent(ev("e", 50, { worker_id: 0 }), lanes, workerIds)).toBe(42);
    expect(resolveTaskForEvent(ev("e", 50, { worker_id: 1 }), lanes, workerIds)).toBe(99);
  });
  it("returns null on an ambiguous cross-worker scan", () => {
    // Concurrent polls on both workers with different tasks -> null.
    expect(resolveTaskForEvent(ev("e", 50, {}), lanes, workerIds)).toBeNull();
  });
  it("resolves an unambiguous cross-worker scan", () => {
    const one: Record<number, WorkerLane> = { 0: lane([poll(0, 100, 42)]) };
    expect(resolveTaskForEvent(ev("e", 50, {}), one, [0])).toBe(42);
  });
});

describe("resolvePollForEvent", () => {
  const lanes: Record<number, WorkerLane> = { 0: lane([poll(0, 100, 42)]) };
  it("returns the actual poll for a worker_id hit", () => {
    const p = resolvePollForEvent(ev("e", 50, { worker_id: 0 }), lanes, [0]);
    expect(p).not.toBeNull();
    expect(p!.taskId).toBe(42);
  });
  it("honors preferTask (mismatch -> null)", () => {
    expect(resolvePollForEvent(ev("e", 50, { worker_id: 0 }), lanes, [0], 99)).toBeNull();
    expect(resolvePollForEvent(ev("e", 50, { worker_id: 0 }), lanes, [0], 42)!.taskId).toBe(42);
  });
});

describe("resolveClusterTask", () => {
  it("returns the common task, else null", () => {
    const taskOf = (e: CustomTraceEvent): number | null =>
      e.name === "x" ? 1 : e.name === "y" ? 2 : null;
    expect(resolveClusterTask([ev("x", 1), ev("x", 2)], taskOf)).toBe(1);
    expect(resolveClusterTask([ev("x", 1), ev("y", 2)], taskOf)).toBeNull(); // disagree
    expect(resolveClusterTask([ev("x", 1), ev("z", 2)], taskOf)).toBeNull(); // one null
  });
});

// ── Highlight-task derivation ────────────────────────────────────────────

describe("eventHighlightTask", () => {
  it("prefers the selected task, then the pinned event's task, else null", () => {
    expect(eventHighlightTask({ selectedTaskId: 5, pinnedEvent: null })).toBe(5);
    expect(
      eventHighlightTask({
        selectedTaskId: null,
        pinnedEvent: {
          events: [],
          timestamp: 0,
          taskId: 9,
          name: "e",
          poll: null,
          detailEvent: null,
        },
      }),
    ).toBe(9);
    expect(eventHighlightTask({ selectedTaskId: null, pinnedEvent: null })).toBeNull();
  });
});
