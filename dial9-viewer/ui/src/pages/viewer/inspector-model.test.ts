// Pure-model tests for the persistent inspector: poll detail, event detail,
// related sections, spawned tasks, and tab availability. The store/DOM-boundary
// halves (tab activation, resize persistence, Esc, the row-walker) are
// browser-driven.

import { describe, it, expect } from "vitest";
import {
  buildEventDetail,
  buildPollDetail,
  buildRelated,
  buildSpawnedTasksView,
  hasNoSelection,
  preferredTab,
  resolveTaskDumpCaptures,
  tabAvailability,
  type RelatedContext,
  type RelatedUiState,
} from "./inspector-model.js";
import type {
  CallframeSymbols,
  CpuSample,
  CustomTraceEvent,
  PollSpan,
  SymbolFrame,
  TracingSpan,
} from "../../lib/trace/index.js";
import type { PinnedCustomEvent, SelectionSlice } from "../../types/state.js";

// ── Fixtures ──────────────────────────────────────────────────────────────

const SYMS: CallframeSymbols = new Map<string, SymbolFrame>([
  ["0x1", { symbol: "leafA", location: "a.rs:1" }],
  ["0x2", { symbol: "midB", location: "b.rs:2" }],
  ["0x3", { symbol: "midC", location: "c.rs:3" }],
  ["0x4", { symbol: "midD", location: "d.rs:4" }],
  ["0x5", { symbol: "rootE", location: "e.rs:5" }],
  ["0x6", { symbol: "leafF", location: "f.rs:6" }],
]);

function sample(callchain: string[]): CpuSample {
  return { callchain } as unknown as CpuSample;
}

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
    fieldKinds: null,
  };
}

function pinnedSingle(e: CustomTraceEvent, taskId: number | null): PinnedCustomEvent {
  return { events: [e], timestamp: e.timestamp, taskId, name: e.name, poll: null, detailEvent: e };
}

function sel(over: Partial<SelectionSlice>): SelectionSlice {
  return {
    selectedTaskId: null,
    spanFocus: null,
    focusedSpanId: null,
    pinnedEvent: null,
    pollDetail: null,
    taskDump: null,
    sidebarRange: null,
    hoveredWakerTaskId: null,
    spawnedTasksRange: null,
    ...over,
  };
}

// ── Poll Detail ────────────────────────────────────────────────────────────

describe("buildPollDetail", () => {
  const stackLong = ["0x1", "0x2", "0x3", "0x4", "0x5"]; // 5 frames
  const stackShort = ["0x6", "0x2"]; // 2 frames
  const stackCpu = ["0x1", "0x2", "0x3", "0x4"]; // 4 frames
  const poll: PollSpan = {
    start: 0,
    end: 1_000_000,
    taskId: 7,
    spawnLocId: null,
    spawnLoc: null,
    schedSamples: [sample(stackLong), sample(stackLong), sample(stackLong), sample(stackShort)],
    cpuSamples: [sample(stackCpu), sample(stackCpu)],
  };

  it("titles with duration + section counts (legacy showStackPopup)", () => {
    const v = buildPollDetail(poll, SYMS);
    expect(v.cpuCount).toBe(2);
    expect(v.schedCount).toBe(4);
    expect(v.title.startsWith("Poll ")).toBe(true);
    expect(v.title).toContain("· 2 CPU samples · 4 sched events");
  });

  it("sched groups: percentages, bar width, and the 3-frame head/expand split", () => {
    const v = buildPollDetail(poll, SYMS);
    expect(v.schedGroups).toHaveLength(2); // sorted by count desc
    const g0 = v.schedGroups[0]!;
    expect(g0.count).toBe(3);
    expect(g0.pct).toBe(75); // round(3/4*100)
    expect(g0.barW).toBe(150); // max(2, 0.75*200)
    expect(g0.leaf).toBe("leafA a.rs:1");
    expect(g0.headFrames).toHaveLength(3); // frames.slice(0,3)
    expect(g0.moreFrames).toHaveLength(2); // frames beyond 3 (5-frame stack)

    const g1 = v.schedGroups[1]!;
    expect(g1.count).toBe(1);
    expect(g1.pct).toBe(25);
    expect(g1.barW).toBe(50); // max(2, 0.25*200)
    expect(g1.headFrames).toHaveLength(2); // 2-frame stack
    expect(g1.moreFrames).toHaveLength(0); // <= 3 frames, nothing to expand
  });

  it("CPU groups start context at frame 1 and carry no summary bar", () => {
    const v = buildPollDetail(poll, SYMS);
    expect(v.cpuGroups).toHaveLength(1);
    const c = v.cpuGroups[0]!;
    expect(c.count).toBe(2);
    expect(c.pct).toBe(100);
    expect(c.barW).toBe(0); // CPU groups have no bar
    expect(c.headFrames).toHaveLength(2); // frames.slice(1,3) of a 4-frame stack
    expect(c.moreFrames).toHaveLength(1); // frames.slice(3)
  });

  it("a poll with no samples yields empty sections", () => {
    const bare: PollSpan = { ...poll, cpuSamples: [], schedSamples: [] };
    const v = buildPollDetail(bare, SYMS);
    expect(v.title).toBe(`Poll ${v.durationLabel}`);
    expect(v.schedGroups).toHaveLength(0);
    expect(v.cpuGroups).toHaveLength(0);
  });

  it("retains the raw, un-deduplicated samples for the flamegraph toggle", () => {
    const v = buildPollDetail(poll, SYMS);
    // The list view deduped 4 sched samples into 2 groups; the raw arrays keep
    // every sample so the flamegraph builds the real tree.
    expect(v.schedSamplesRaw).toHaveLength(4);
    expect(v.cpuSamplesRaw).toHaveLength(2);
    // And they carry the callchain the deduped groups threw away.
    expect(v.cpuSamplesRaw[0]!.callchain).toEqual(stackCpu);
  });

  it("drops samples with no stack from the raw arrays", () => {
    const withEmpty: PollSpan = {
      ...poll,
      cpuSamples: [sample(stackCpu), sample([])],
      schedSamples: [sample([])],
    };
    const v = buildPollDetail(withEmpty, SYMS);
    // A flamegraph over an empty callchain builds a degenerate tree, so the
    // stackless samples are filtered out.
    expect(v.cpuSamplesRaw).toHaveLength(1);
    expect(v.schedSamplesRaw).toHaveLength(0);
  });

  it("a poll with no samples yields empty raw arrays (no throw)", () => {
    const bare: PollSpan = { ...poll, cpuSamples: [], schedSamples: [] };
    const v = buildPollDetail(bare, SYMS);
    expect(v.cpuSamplesRaw).toEqual([]);
    expect(v.schedSamplesRaw).toEqual([]);
  });
});

// ── Event detail ─────────────────────────────────────────────────────────────

describe("buildEventDetail", () => {
  const fmtTs = (ns: number): string => `t${ns}`;

  it("single event: fields + correlation only on shared values + @ + Task", () => {
    const e = ev("Req", 100, { path: "/a", id: 42 });
    const all = [e, ev("Req", 200, { path: "/a", id: 99 })]; // shares path=/a, not id
    const v = buildEventDetail(pinnedSingle(e, 42), all, fmtTs);
    expect(v.title).toBe("Req");
    expect(v.isSingle).toBe(true);
    const path = v.rows.find((r) => r.key === "path")!;
    const id = v.rows.find((r) => r.key === "id")!;
    expect(path.corrVal).toBe("/a"); // shared -> correlation offered
    expect(id.corrVal).toBeNull(); // unique -> no correlation
    expect(path.chart).toBe(false);
    expect(id.chart).toBe(true);
    expect(v.rows.find((r) => r.key === "@")!.value).toBe("t100");
    expect(v.rows.find((r) => r.key === "@")!.chart).toBe(false);
    expect(v.rows.find((r) => r.key === "Task")!.value).toBe("0x2a (selected)");
  });

  it("cluster: count / types / range rows, no correlation, Related disabled", () => {
    const a = ev("A", 100);
    const b = ev("B", 300);
    const pinned: PinnedCustomEvent = {
      events: [a, b],
      timestamp: 100,
      taskId: null,
      name: "A",
      poll: null,
      detailEvent: null,
    };
    const v = buildEventDetail(pinned, [a, b], fmtTs);
    expect(v.isSingle).toBe(false);
    expect(v.title).toBe("Cluster · 2 events");
    expect(v.rows.find((r) => r.key === "Cluster")!.value).toBe("2 events");
    expect(v.rows.find((r) => r.key === "@")!.value).toBe("t100 – t300");
    expect(v.rows.every((r) => r.corrVal === null)).toBe(true);
  });

  it("does not offer charts for comma-delimited event or field names", () => {
    const commaEvent = ev("Req,Finished", 100, { value: 42 });
    const commaField = ev("Req", 200, { "value,total": 42 });

    expect(
      buildEventDetail(
        pinnedSingle(commaEvent, null),
        [commaEvent],
        fmtTs,
      ).rows.find((row) => row.key === "value")?.chart,
    ).toBe(false);
    expect(
      buildEventDetail(
        pinnedSingle(commaField, null),
        [commaField],
        fmtTs,
      ).rows.find((row) => row.key === "value,total")?.chart,
    ).toBe(false);
  });
});

// ── Related ──────────────────────────────────────────────────────────────────

describe("buildRelated", () => {
  const anchor = ev("Tick", 100, { path: "/x" });
  const sameType = ev("Tick", 300, { path: "/x" });
  const other = ev("Other", 150);
  const all = [anchor, sameType, other];
  const ctx: RelatedContext = {
    allEvents: all,
    allSpans: [],
    taskOf: (e) => (e.name === "Tick" ? 5 : null),
    fmtTs: (ns) => `t${ns}`,
    fmtDelta: (ns) => (ns >= 0 ? `+${ns}` : `${ns}`),
  };
  const restingUi: RelatedUiState = { collapsed: {}, expand: {}, correlate: null };

  it("emits the standard sections with counts and empty states", () => {
    const v = buildRelated(anchor, ctx, restingUi);
    const titles = v.sections.map((s) => s.title);
    expect(titles).toContain("Enclosing spans");
    expect(titles).toContain("Same task");
    expect(titles).toContain("Same type");
    // No spans -> Enclosing spans is an empty section.
    const enc = v.sections.find((s) => s.title === "Enclosing spans")!;
    expect(enc.count).toBe(0);
    expect(enc.empty).toBe("none");
    // Same type: two "Tick" events (anchor + sameType).
    const st = v.sections.find((s) => s.title === "Same type")!;
    expect(st.count).toBe(2);
    // The self row is non-navigable.
    const selfRow = st.rows.find((r) => r.self);
    expect(selfRow?.target).toBeNull();
  });

  it("adds the field-correlation section only when the value links", () => {
    const withCorr: RelatedUiState = { ...restingUi, correlate: { key: "path", val: "/x" } };
    const v = buildRelated(anchor, ctx, withCorr);
    expect(v.sections.some((s) => s.title === "Same path=/x")).toBe(true);
  });

  it("task-unresolved renders the placeholder", () => {
    const v = buildRelated(other, ctx, restingUi);
    const sameTask = v.sections.find((s) => s.title === "Same task")!;
    expect(sameTask.empty).toBe("task unresolved");
  });
});

// ── Spawned tasks ────────────────────────────────────────────────────────────

describe("buildSpawnedTasksView", () => {
  it("keeps 5 task links per group + a 'N more' tail", () => {
    const tasks = Array.from({ length: 7 }, (_, i) => ({ taskId: i + 1, firstPoll: i }));
    const result = { total: 7, groups: [{ loc: "spawn.rs:1", tasks }] };
    const v = buildSpawnedTasksView(result, "1.0ms")!;
    expect(v.total).toBe(7);
    expect(v.rangeLabel).toBe("1.0ms");
    expect(v.groups[0]!.head).toHaveLength(5);
    expect(v.groups[0]!.head[0]!.hex).toBe("0x1");
    expect(v.groups[0]!.moreCount).toBe(2);
  });

  it("returns null when the range found no task (legacy early return)", () => {
    expect(buildSpawnedTasksView(null, "1.0ms")).toBeNull();
  });
});

// ── Tab families + activation ────────────────────────────────────────────────
describe("task-dump selection resolution", () => {
  it("re-resolves timestamps against the current trace after replacement", () => {
    const oldDump = { timestamp: 3, callchain: ["old"] };
    const newDump = { timestamp: 3, callchain: ["new"] };
    const selection = { taskId: 9, timestamps: [3] };
    const traceWith = (dump: typeof oldDump | null) =>
      ({
        taskDumps: new Map(dump === null ? [] : [[9, [dump]]]),
      }) as Parameters<typeof resolveTaskDumpCaptures>[0];

    expect(resolveTaskDumpCaptures(traceWith(oldDump), selection)).toEqual([oldDump]);
    expect(resolveTaskDumpCaptures(traceWith(null), selection)).toEqual([]);
    const resolved = resolveTaskDumpCaptures(traceWith(newDump), selection);
    expect(resolved).toEqual([newDump]);
    expect(resolved[0]).not.toBe(oldDump);
  });
});

describe("tab availability + preferred tab", () => {
  it("a poll click enables + prefers Poll", () => {
    const poll = { start: 0, end: 10 } as PollSpan;
    const s = sel({ pollDetail: poll });
    expect(tabAvailability(s).poll).toBe(true);
    expect(preferredTab(s)).toBe("poll");
  });

  it("a single pin enables Event + Related; a cluster enables Event only", () => {
    const single = sel({ pinnedEvent: pinnedSingle(ev("E", 1), 3) });
    expect(tabAvailability(single).event).toBe(true);
    expect(tabAvailability(single).related).toBe(true);
    expect(preferredTab(single)).toBe("event");

    const cluster = sel({
      pinnedEvent: {
        events: [ev("A", 1), ev("B", 2)],
        timestamp: 1,
        taskId: null,
        name: "A",
        poll: null,
        detailEvent: null,
      },
    });
    expect(tabAvailability(cluster).event).toBe(true);
    expect(tabAvailability(cluster).related).toBe(false);
  });

  it("a task select prefers Task; a range or task dump prefers Stack", () => {
    expect(preferredTab(sel({ selectedTaskId: 9 }))).toBe("task");
    expect(preferredTab(sel({ spawnedTasksRange: { startNs: 0, endNs: 5 } }))).toBe("stack");
    expect(preferredTab(sel({ sidebarRange: { startNs: 0, endNs: 5 } }))).toBe("stack");
    const taskDump = { taskId: 9, timestamps: [3] };
    const dumpSelection = sel({ taskDump });
    expect(tabAvailability(dumpSelection).stack).toBe(true);
    expect(preferredTab(dumpSelection)).toBe("stack");
    expect(hasNoSelection(dumpSelection)).toBe(false);
  });

  it("hasNoSelection is true only at rest, and preferredTab is null then", () => {
    expect(hasNoSelection(sel({}))).toBe(true);
    expect(preferredTab(sel({}))).toBeNull();
    expect(hasNoSelection(sel({ selectedTaskId: 1 }))).toBe(false);
  });
});
