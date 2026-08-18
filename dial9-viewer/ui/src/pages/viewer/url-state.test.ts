import { describe, it, expect } from "vitest";
import type { ReadonlyState } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import {
  hydrateViewerStore,
  projectViewerState,
  mirrorViewerToQuery,
  readViewerUrlState,
  VIEWER_URL_SLICES,
} from "./url-state.js";
import { createViewerStore } from "./store.js";

// A store shape carrying only the slices projectViewerState reads. The other
// slices are irrelevant to the projection, so a partial cast keeps the fixture
// small.
function mkState(over: {
  viewport?: Partial<StoreState["viewport"]>;
  selection?: Partial<StoreState["selection"]>;
  uiPrefs?: Partial<StoreState["uiPrefs"]>;
  poi?: Partial<StoreState["poi"]>;
  view?: Partial<StoreState["view"]>;
  trace?: StoreState["trace"];
}): ReadonlyState<StoreState> {
  return {
    viewport: { minTs: 0, maxTs: 1000, viewStart: 0, viewEnd: 1000, ...over.viewport },
    selection: {
      selectedTaskId: null,
      spanFocus: null,
      focusedSpanId: null,
      pinnedEvent: null,
      pollDetail: null,
      taskDump: null,
      sidebarRange: null,
      hoveredWakerTaskId: null,
      spawnedTasksRange: null,
      ...over.selection,
    },
    uiPrefs: {
      panelCollapsed: {},
      trackOrder: [],
      collapsed: {},
      collapsedRuntimes: {},
      collapsedRuntimeMetrics: {},
      sidebarWidth: 360,
      railWidth: 300,
      taskColWidths: {},
      issueColWidths: {},
      lanesViewportHeight: 360,
      lanesScrollTop: 0,
      selectedSpanNames: new Set<string>(),
      selectedEventNames: new Set<string>(),
      spanFilter: "",
      spanPctFilter: 0,
      timeMode: "rel",
      tz: "utc",
      ...over.uiPrefs,
    },
    poi: {
      filter: "sched",
      sortKey: "duration",
      sortDir: "desc",
      index: -1,
      railTab: "issues",
      taskSort: "total",
      taskSortDir: "desc",
      taskIndex: -1,
      ...over.poi,
    },
    view: {
      fieldCharts: [],
      inspectorTab: "task",
      expandedPollGroups: new Set<string>(),
      pollFlamegraphSection: "cpu",
      pollWorkerZoom: [],
      pollOffworkerZoom: [],
      relatedCollapsed: {},
      relatedExpand: {},
      relatedCorrelate: null,
      regionMode: null,
      regionHeapMode: "bytes",
      regionGroupBy: "leaf",
      regionWorkerZoom: [],
      regionOffworkerZoom: [],
      regionInspectFocus: null,
      spanNavIndex: -1,
      ...over.view,
    },
    trace: over.trace ?? { trace: null },
  } as unknown as ReadonlyState<StoreState>;
}

/** project -> mirror -> read: the shape a shared URL round-trips through. */
function roundTrip(state: ReadonlyState<StoreState>) {
  const params = new URLSearchParams();
  mirrorViewerToQuery(params, projectViewerState(state));
  return { params, out: readViewerUrlState("?" + params.toString()) };
}

describe("viewer URL state: issues-rail (poi)", () => {
  it("round-trips a non-default filter, sort, and index", () => {
    const { params, out } = roundTrip(
      mkState({ poi: { filter: "long-poll", sortKey: "time", sortDir: "asc", index: 4 } }),
    );
    expect(params.get("issue")).toBe("long-poll");
    expect(params.get("issue-sort")).toBe("time,asc");
    expect(params.get("issue-index")).toBe("4");
    expect(out.poiFilter).toBe("long-poll");
    expect(out.poiSort).toEqual({ key: "time", dir: "asc" });
    expect(out.poiIndex).toBe(4);
  });

  it("emits nothing for the resting defaults", () => {
    const { params } = roundTrip(mkState({}));
    expect(params.get("issue")).toBeNull();
    expect(params.get("issue-sort")).toBeNull();
    expect(params.get("issue-index")).toBeNull();
  });

  it("omits index -1 (no current POI) but still carries a non-default sort", () => {
    const { params, out } = roundTrip(
      mkState({ poi: { filter: "sched", sortKey: "worker", sortDir: "desc", index: -1 } }),
    );
    expect(params.get("issue-index")).toBeNull();
    expect(out.poiSort).toEqual({ key: "worker", dir: "desc" });
    expect(out.poiIndex).toBeUndefined();
  });

  it("drops a garbage filter / sort on read", () => {
    const out = readViewerUrlState("?issue=bogus&issue-sort=nope,sideways");
    expect(out.poiFilter).toBeUndefined();
    expect(out.poiSort).toBeUndefined();
  });
});

describe("viewer URL state: span filters", () => {
  it("round-trips the percentile filter", () => {
    const { params, out } = roundTrip(mkState({ uiPrefs: { spanPctFilter: 99 } }));
    expect(params.get("span-pct")).toBe("99");
    expect(out.spanPct).toBe(99);
  });

  it("drops an out-of-set percentile", () => {
    expect(readViewerUrlState("?span-pct=42").spanPct).toBeUndefined();
  });

  it("round-trips legend name chips, including a name containing a comma", () => {
    const { out } = roundTrip(
      mkState({
        uiPrefs: {
          selectedSpanNames: new Set(["poll", "http, request"]),
          selectedEventNames: new Set(["flush"]),
        },
      }),
    );
    expect(out.spanNames).toEqual(["http, request", "poll"]);
    expect(out.eventNames).toEqual(["flush"]);
  });
});

describe("viewer URL state: focused span", () => {
  it("round-trips the span-panel subtree focus id independently", () => {
    const { params, out } = roundTrip(mkState({ selection: { focusedSpanId: "0xabc" } }));
    expect(params.get("span-focus")).toBe("0xabc");
    expect(out.focusedSpanId).toBe("0xabc");
  });
});

describe("viewer URL state: dynamic field charts", () => {
  it("round-trips repeatable comma-separated definitions without a version", () => {
    const charts: StoreState["view"]["fieldCharts"] = [
      {
        id: "fc-1",
        eventName: "request.finished",
        fieldName: "bytes_total",
        kind: "counter",
      },
      {
        id: "fc-2",
        eventName: "queue.depth",
        fieldName: "active",
        kind: "updown-counter",
      },
    ];
    const { params, out } = roundTrip(mkState({ view: { fieldCharts: charts } }));

    expect(params.getAll("field-chart")).toEqual([
      "fc-1,request.finished,bytes_total,counter",
      "fc-2,queue.depth,active,updown-counter",
    ]);
    expect(out.fieldCharts).toEqual(charts);
  });

  it("round-trips literal percent escapes in event and field names", () => {
    const charts: StoreState["view"]["fieldCharts"] = [
      {
        id: "fc-percent",
        eventName: "request 50%",
        fieldName: "bytes %09",
        kind: "gauge",
      },
    ];

    const { params, out } = roundTrip(mkState({ view: { fieldCharts: charts } }));
    expect(params.get("field-chart")).toBe(
      "fc-percent,request 50%,bytes %09,gauge",
    );
    expect(out.fieldCharts).toEqual(charts);
  });

  it("drops malformed definitions and duplicate ids", () => {
    const valid = "fc-a,Metric,value,gauge";
    const params = new URLSearchParams();
    params.append("field-chart", "legacy,shape");
    params.append("field-chart", "bad-id,Metric,value,gauge");
    params.append("field-chart", "fc-b,Metric,value,total,gauge");
    params.append("field-chart", "fc-c,Metric,value,histogram");
    params.append("field-chart", valid);
    params.append("field-chart", valid);

    expect(readViewerUrlState(`?${params.toString()}`).fieldCharts).toEqual([
      {
        id: "fc-a",
        eventName: "Metric",
        fieldName: "value",
        kind: "gauge",
      },
    ]);
  });
});

describe("viewer URL state: task dump", () => {
  it("round-trips the selected task and capture timestamps", () => {
    const { params, out } = roundTrip(
      mkState({
        selection: {
          selectedTaskId: 7,
          taskDump: { taskId: 7, timestamps: [101, 205] },
        },
        view: { inspectorTab: "stack" },
      }),
    );
    expect(params.get("task-dump")).toBe("7:101,205");
    expect(out.taskDump).toEqual({ taskId: 7, timestamps: [101, 205] });
  });
});

describe("viewer URL state: embedded flamegraph focus", () => {
  it("round-trips a region flamegraph inspect focus", () => {
    const { params, out } = roundTrip(
      mkState({
        view: {
          regionInspectFocus: "tokio::runtime::task::harness::poll_future",
        },
      }),
    );
    expect(params.get("analysis-inspect")).toBe(
      "tokio::runtime::task::harness::poll_future",
    );
    expect(out.regionInspectFocus).toBe(
      "tokio::runtime::task::harness::poll_future",
    );
  });
});


describe("viewer URL state: complete durable view", () => {
  it("round-trips rail, layout, inspector, disclosure, analysis, zoom, and cursor state", () => {
    const { params, out } = roundTrip(
      mkState({
        poi: {
          railTab: "tasks",
          taskSort: "lifetime",
          taskSortDir: "asc",
          taskIndex: 7,
        },
        uiPrefs: {
          collapsedRuntimes: { beta: true, alpha: true },
          sidebarWidth: 444,
          railWidth: 460,
          taskColWidths: { polls: 48, loc: 260 },
          issueColWidths: { kind: 120, dot: 14 },
          lanesViewportHeight: 280,
          lanesScrollTop: 96,
          stacksAsFlamegraph: true,
        },
        view: {
          inspectorTab: "related",
          expandedPollGroups: new Set(["sched-1", "cpu-0"]),
          pollFlamegraphSection: "sched",
          pollWorkerZoom: ["root", "poll"],
          pollOffworkerZoom: ["off", "wait"],
          relatedCollapsed: { "Same task": true },
          relatedExpand: { "Same span": { before: 25, after: 50 } },
          relatedCorrelate: { key: "request,id", val: "abc/123" },
          regionMode: "heap",
          regionHeapMode: "count",
          regionGroupBy: "full",
          regionWorkerZoom: ["root", "alloc"],
          regionOffworkerZoom: ["off", "alloc"],
          spanNavIndex: 5,
        },
      }),
    );

    expect(params.get("rail")).toBe("tasks");
    expect(params.get("rail-width")).toBe("460");
    expect(params.get("task-cols")).toBe("v1:loc,260\tpolls,48");
    expect(params.get("issue-cols")).toBe("v1:dot,14\tkind,120");
    expect(params.get("task-sort")).toBe("lifetime,asc");
    expect(params.get("runtime-collapsed")).toBe("v1:alpha\tbeta");
    expect(params.get("poll-worker-zoom")).toBe("root\tpoll");
    expect(params.get("analysis")).toBe("heap");
    expect(out).toMatchObject({
      railTab: "tasks",
      taskSort: { key: "lifetime", dir: "asc" },
      taskIndex: 7,
      collapsedRuntimes: ["alpha", "beta"],
      inspectorWidth: 444,
      railWidth: 460,
      taskColWidths: { loc: 260, polls: 48 },
      issueColWidths: { dot: 14, kind: 120 },
      lanesHeight: 280,
      lanesScrollTop: 96,
      stacksAsFlamegraph: true,
      inspectorTab: "related",
      pollSection: "sched",
      expandedPollGroups: ["cpu-0", "sched-1"],
      pollWorkerZoom: ["root", "poll"],
      pollOffworkerZoom: ["off", "wait"],
      relatedCollapsed: ["Same task"],
      relatedExpand: { "Same span": { before: 25, after: 50 } },
      relatedCorrelate: { key: "request,id", val: "abc/123" },
      regionMode: "heap",
      regionHeapMode: "count",
      regionGroupBy: "full",
      regionWorkerZoom: ["root", "alloc"],
      regionOffworkerZoom: ["off", "alloc"],
      spanNavIndex: 5,
    });
  });

  it("keeps Set Range data filtering distinct from viewport position", () => {
    const trace = {
      filterStartTime: 100,
      filterEndTime: 900,
    } as NonNullable<StoreState["trace"]["trace"]>;
    const { params, out } = roundTrip(
      mkState({
        trace: { trace },
        viewport: { minTs: 100, maxTs: 900, viewStart: 200, viewEnd: 300 },
      }),
    );
    expect(params.get("data-start")).toBe("100");
    expect(params.get("data-end")).toBe("900");
    expect(params.get("start")).toBe("200");
    expect(params.get("end")).toBe("300");
    expect(out.dataRange).toEqual({ startNs: 100, endNs: 900 });
    expect([out.viewStart, out.viewEnd]).toEqual([200, 300]);
  });

  it("omits every durable resting default", () => {
    const { params } = roundTrip(mkState({}));
    expect([...params.keys()]).toEqual([]);
  });

  it("drops malformed enums, paths, ranges, dimensions, and disclosure entries", () => {
    const out = readViewerUrlState(
      "?rail=nope&task-sort=wat,sideways&task-index=-2" +
        "&inspector=nope&analysis=wat&heap-weight=wat&blocking-group=wat" +
        "&poll-worker-zoom=root%09%09leaf&lanes-height=0&inspector-width=nan" +
        "&related-expand=bad&data-start=900&data-end=100",
    );
    expect(out).toEqual({});
  });
});


describe("viewer URL state: constructible and strict values", () => {
  it("reads a single-pass agent-authored list containing commas", () => {
    const out = readViewerUrlState("?span-names=v1%3Ahttp%2C+request%09poll");
    expect(out.spanNames).toEqual(["http, request", "poll"]);
  });

  it("continues to read the previously emitted double-encoded comma grammar", () => {
    const out = readViewerUrlState("?span-names=http%252C%2520request,poll");
    expect(out.spanNames).toEqual(["http, request", "poll"]);
  });

  it("drops blank, fractional, negative, incomplete, and trailing-junk integers", () => {
    const out = readViewerUrlState(
      "?task=0x10junk&poll=100:2.5&issue-index=1.6&task-index=-1" +
        "&span-index=&lanes-scroll=+2&data-start=&data-end=900",
    );
    expect(out).toEqual({ dataRange: { endNs: 900 } });
  });

  it("emits an explicit Task tab when a retained poll would otherwise auto-open Poll", () => {
    const state = mkState({
      selection: {
        pollDetail: { start: 10, end: 20, taskId: 7 } as StoreState["selection"]["pollDetail"],
      },
      view: { inspectorTab: "task" },
    });
    const params = new URLSearchParams();
    mirrorViewerToQuery(params, projectViewerState(state));
    expect(params.get("inspector")).toBe("task");
  });
});

describe("viewer URL state: store hydration", () => {
  it("applies decoded durable controls through one boot entry point", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const decoded = readViewerUrlState(
      "?rail=tasks&task-sort=lifetime,asc&inspector=stack" +
        "&analysis=cpu&analysis-inspect=tokio%3A%3Apoll" +
        "&stack-view=flame&inspector-width=444" +
        "&field-chart=fc-1%2CMetric%2Cvalue%2Ccounter",
    );

    hydrateViewerStore(store, decoded, {
      timeMode: "abs",
      timeZone: "local",
    });

    expect(store.getState().poi).toMatchObject({
      railTab: "tasks",
      taskSort: "lifetime",
      taskSortDir: "asc",
    });
    expect(store.getState().uiPrefs).toMatchObject({
      timeMode: "abs",
      tz: "local",
      stacksAsFlamegraph: true,
      sidebarWidth: 444,
    });
    expect(store.getState().view).toMatchObject({
      fieldCharts: [
        {
          id: "fc-1",
          eventName: "Metric",
          fieldName: "value",
          kind: "counter",
        },
      ],
      inspectorTab: "stack",
      regionMode: "cpu",
      regionInspectFocus: "tokio::poll",
    });
  });

  it("derives the binding slices from field ownership", () => {
    expect([...VIEWER_URL_SLICES].sort()).toEqual(
      ["trace", "viewport", "selection", "poi", "uiPrefs", "view"].sort(),
    );
  });
});
