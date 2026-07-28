import { describe, it, expect } from "vitest";
import type { ReadonlyState } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import {
  projectViewerState,
  mirrorViewerToQuery,
  readViewerUrlState,
} from "./url-state.js";

// A store shape carrying only the slices projectViewerState reads. The other
// slices are irrelevant to the projection, so a partial cast keeps the fixture
// small.
function mkState(over: {
  viewport?: Partial<StoreState["viewport"]>;
  selection?: Partial<StoreState["selection"]>;
  uiPrefs?: Partial<StoreState["uiPrefs"]>;
  poi?: Partial<StoreState["poi"]>;
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
      sidebarWidth: 320,
      selectedSpanNames: new Set<string>(),
      selectedEventNames: new Set<string>(),
      spanFilter: "",
      spanPctFilter: 0,
      timeMode: "rel",
      tz: "utc",
      ...over.uiPrefs,
    },
    poi: { filter: "sched", sortKey: "duration", sortDir: "desc", index: -1, ...over.poi },
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
    expect(out.spanNames).toEqual(["poll", "http, request"]);
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
