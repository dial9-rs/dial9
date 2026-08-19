// Tests for track management. Env is node (no DOM): the drag-and-drop wiring is
// the browser-check half; here we pin the pure surface + store actions + the
// persistence round-trip ("uiPrefs survives reload"):
//   1. manageability + order resolution (robust to stale/partial orders).
//   2. collapse predicate + swap-reorder.
//   3. store actions dispatch the right uiPrefs updates (no thrash).
//   4. persistence: save -> reload -> hydrate restores order + collapse.
//   5. a collapsed track stays in the ordered list, so re-expand restores it
//      (windowing respected on re-expand).

import { afterEach, describe, expect, it, vi } from "vitest";
import { createViewerStore } from "./store.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import { TRACKS } from "../../lib/canvas/track-layout.js";
import type { TrackId } from "../../lib/canvas/track-layout.js";
import {
  COLLAPSED_TRACK_H,
  MANAGEABLE_TRACK_IDS,
  TRACK_PREFS_STORAGE_KEY,
  computeReorder,
  createTrackManageActions,
  hydrateTrackPrefs,
  isCollapsed,
  isManageableTrack,
  loadTrackPrefs,
  mountTrackPrefsPersistence,
  orderedTracks,
  saveTrackPrefs,
} from "./track-management.js";

const CATALOGUE_ORDER: TrackId[] = TRACKS.map((t) => t.id);

/** A manual frame scheduler: collect pending flushes, run them on demand. */
function manualScheduler(): { flush(): void; store: ViewerStore } {
  const pending: (() => void)[] = [];
  const store = createViewerStore({
    scheduler: (cb) => {
      pending.push(cb);
    },
  });
  return {
    store,
    flush(): void {
      const runnable = pending.splice(0);
      for (const cb of runnable) cb();
    },
  };
}

function uiPrefs(store: ViewerStore): StoreState["uiPrefs"] {
  return store.getState().uiPrefs;
}

/** In-memory localStorage stand-in (node has none); optionally throwing. */
function fakeLocalStorage(opts?: { throwOnAccess?: boolean }): Storage {
  const m = new Map<string, string>();
  const guard = <T>(fn: () => T): T => {
    if (opts?.throwOnAccess) throw new Error("storage blocked");
    return fn();
  };
  return {
    get length() {
      return m.size;
    },
    clear: () => guard(() => m.clear()),
    getItem: (k: string) => guard(() => (m.has(k) ? (m.get(k) ?? null) : null)),
    setItem: (k: string, v: string) => guard(() => void m.set(k, String(v))),
    removeItem: (k: string) => guard(() => void m.delete(k)),
    key: (i: number) => guard(() => [...m.keys()][i] ?? null),
  } as Storage;
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("manageable tracks", () => {
  it("collapse/reorder apply to the four foldable analysis tracks only", () => {
    expect([...MANAGEABLE_TRACK_IDS].sort()).toEqual(
      ["cpu", "events", "queue", "spans"].sort(),
    );
    for (const id of ["cpu", "queue", "spans", "events"] as TrackId[]) {
      expect(isManageableTrack(id)).toBe(true);
    }
    for (const id of ["timeline", "lanes", "task-detail"] as TrackId[]) {
      expect(isManageableTrack(id)).toBe(false);
    }
  });
});

describe("orderedTracks", () => {
  it("resolves empty order to the catalogue order", () => {
    expect(orderedTracks([]).map((t) => t.id)).toEqual(CATALOGUE_ORDER);
  });

  it("permutes manageable tracks, pinning structural + task-detail slots", () => {
    // Reverse the manageable block; timeline/lanes/task-detail keep their
    // catalogue slots (task-detail sits directly under lanes).
    const ids = orderedTracks(["events", "spans", "queue", "cpu"]).map((t) => t.id);
    expect(ids).toEqual([
      "timeline",
      "lanes",
      "task-detail",
      "events",
      "spans",
      "queue",
      "cpu",
    ]);
  });

  it("is robust to unknown ids, non-manageable ids, and duplicates", () => {
    const ids = orderedTracks([
      "nope", // unknown -> ignored
      "lanes", // non-manageable -> ignored (stays pinned)
      "spans",
      "spans", // duplicate -> ignored
      "cpu",
    ]).map((t) => t.id);
    // spans, cpu named first; queue + events (unnamed) appended in catalogue
    // order; structural/task-detail slots unchanged.
    expect(ids).toEqual([
      "timeline",
      "lanes",
      "task-detail",
      "spans",
      "cpu",
      "queue",
      "events",
    ]);
  });

  it("appends a manageable track missing from a stale stored order", () => {
    // Simulates an order saved before a track existed: only two named.
    const ids = orderedTracks(["events", "cpu"]).map((t) => t.id);
    expect(ids).toEqual([
      "timeline",
      "lanes",
      "task-detail",
      "events",
      "cpu",
      "queue", // unnamed, appended in catalogue order
      "spans",
    ]);
  });
});

describe("isCollapsed", () => {
  it("is true only for a manageable track explicitly flagged", () => {
    expect(isCollapsed({ cpu: true }, "cpu")).toBe(true);
    expect(isCollapsed({ cpu: false }, "cpu")).toBe(false);
    expect(isCollapsed({}, "cpu")).toBe(false);
  });

  it("ignores the flag for non-manageable tracks", () => {
    expect(isCollapsed({ lanes: true }, "lanes")).toBe(false);
    expect(isCollapsed({ "task-detail": true }, "task-detail")).toBe(false);
  });
});

describe("computeReorder (drop = swap position)", () => {
  it("swaps the two tracks' positions", () => {
    // Catalogue manageable order: cpu, queue, spans, events. Swap cpu<->spans.
    expect(computeReorder([], "cpu", "spans")).toEqual([
      "spans",
      "queue",
      "cpu",
      "events",
    ]);
  });

  it("is a no-op for the same track", () => {
    expect(computeReorder([], "cpu", "cpu")).toEqual([
      "cpu",
      "queue",
      "spans",
      "events",
    ]);
  });

  it("ignores a non-manageable dragged or target track", () => {
    expect(computeReorder([], "cpu", "lanes" as TrackId)).toEqual([
      "cpu",
      "queue",
      "spans",
      "events",
    ]);
    expect(computeReorder([], "timeline" as TrackId, "cpu")).toEqual([
      "cpu",
      "queue",
      "spans",
      "events",
    ]);
  });

  it("swaps within an already-custom order", () => {
    // Start from a reversed order, swap the first two.
    expect(computeReorder(["events", "spans", "queue", "cpu"], "events", "spans")).toEqual([
      "spans",
      "events",
      "queue",
      "cpu",
    ]);
  });
});

describe("store actions", () => {
  it("toggleCollapse flips a manageable track's collapse flag", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const actions = createTrackManageActions(store);
    expect(uiPrefs(store).collapsed["cpu"]).toBeUndefined();
    actions.toggleCollapse("cpu");
    expect(uiPrefs(store).collapsed["cpu"]).toBe(true);
    actions.toggleCollapse("cpu");
    expect(uiPrefs(store).collapsed["cpu"]).toBe(false);
  });

  it("toggleCollapse ignores non-manageable tracks", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const actions = createTrackManageActions(store);
    actions.toggleCollapse("lanes");
    expect(uiPrefs(store).collapsed["lanes"]).toBeUndefined();
  });

  it("reorder swaps two tracks and writes trackOrder", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const actions = createTrackManageActions(store);
    actions.reorder("cpu", "events");
    expect(uiPrefs(store).trackOrder).toEqual(["events", "queue", "spans", "cpu"]);
  });

  it("reorder is a no-op (no store write) when nothing changes", () => {
    const sched = manualScheduler();
    const actions = createTrackManageActions(sched.store);
    let notifications = 0;
    sched.store.subscribe(["uiPrefs"], () => {
      notifications += 1;
    });
    actions.reorder("cpu", "cpu"); // same track -> no change
    sched.flush();
    expect(notifications).toBe(0);
  });
});

describe("persistence: uiPrefs survives reload (headline DoD)", () => {
  it("save -> hydrate restores track order + collapse across a fresh store", () => {
    vi.stubGlobal("localStorage", fakeLocalStorage());

    // Session 1: reorder + collapse, with the persistence subscriber mounted.
    const s1 = manualScheduler();
    const dispose = mountTrackPrefsPersistence(s1.store);
    const actions = createTrackManageActions(s1.store);
    actions.reorder("cpu", "events"); // -> events, queue, spans, cpu
    actions.toggleCollapse("spans"); // -> spans collapsed
    s1.flush(); // subscriber writes to localStorage
    dispose();

    // Simulate reload: a brand-new store hydrated from the same storage.
    const s2 = createViewerStore({ scheduler: () => {} });
    expect(uiPrefs(s2).trackOrder).toEqual([]); // fresh default before hydrate
    hydrateTrackPrefs(s2);
    expect(uiPrefs(s2).trackOrder).toEqual(["events", "queue", "spans", "cpu"]);
    expect(uiPrefs(s2).collapsed["spans"]).toBe(true);

    // And the resolved column order reflects the restored trackOrder.
    expect(orderedTracks(uiPrefs(s2).trackOrder).map((t) => t.id)).toEqual([
      "timeline",
      "lanes",
      "task-detail",
      "events",
      "queue",
      "spans",
      "cpu",
    ]);
  });

  it("loadTrackPrefs returns null when nothing is stored", () => {
    vi.stubGlobal("localStorage", fakeLocalStorage());
    expect(loadTrackPrefs()).toBeNull();
  });

  it("hydrate is a no-op when nothing is stored (keeps defaults)", () => {
    vi.stubGlobal("localStorage", fakeLocalStorage());
    const store = createViewerStore({ scheduler: () => {} });
    hydrateTrackPrefs(store);
    expect(uiPrefs(store).trackOrder).toEqual([]);
    expect(uiPrefs(store).collapsed).toEqual({});
  });

  it("degrades a malformed stored blob to null instead of throwing", () => {
    const ls = fakeLocalStorage();
    ls.setItem(TRACK_PREFS_STORAGE_KEY, "{not json");
    vi.stubGlobal("localStorage", ls);
    expect(loadTrackPrefs()).toBeNull();
  });

  it("drops non-string order entries and non-boolean collapse values", () => {
    const ls = fakeLocalStorage();
    ls.setItem(
      TRACK_PREFS_STORAGE_KEY,
      JSON.stringify({ trackOrder: ["cpu", 3, null, "spans"], collapsed: { cpu: true, queue: 1 } }),
    );
    vi.stubGlobal("localStorage", ls);
    const prefs = loadTrackPrefs();
    expect(prefs?.trackOrder).toEqual(["cpu", "spans"]);
    expect(prefs?.collapsed).toEqual({ cpu: true });
  });

  it("falls back to an in-memory store when localStorage throws", () => {
    vi.stubGlobal("localStorage", fakeLocalStorage({ throwOnAccess: true }));
    // save writes to the memory fallback; load reads it back (no throw).
    saveTrackPrefs({ trackOrder: ["queue", "cpu", "spans", "events"], collapsed: { queue: true } });
    const prefs = loadTrackPrefs();
    expect(prefs?.trackOrder).toEqual(["queue", "cpu", "spans", "events"]);
    expect(prefs?.collapsed["queue"]).toBe(true);
  });

  it("persists + restores the lanes box height across a fresh store", () => {
    vi.stubGlobal("localStorage", fakeLocalStorage());
    const s1 = manualScheduler();
    const dispose = mountTrackPrefsPersistence(s1.store);
    s1.store.update("uiPrefs", { lanesViewportHeight: 312 });
    s1.flush(); // subscriber writes to localStorage
    dispose();

    const s2 = createViewerStore({ scheduler: () => {} });
    expect(uiPrefs(s2).lanesViewportHeight).toBe(360); // store default before hydrate
    hydrateTrackPrefs(s2);
    expect(uiPrefs(s2).lanesViewportHeight).toBe(312);
  });

  it("never writes dynamic chart order or collapse state to localStorage", () => {
    vi.stubGlobal("localStorage", fakeLocalStorage());
    saveTrackPrefs({
      trackOrder: ["events", "fc-1", "fc-future.v2", "cpu"],
      collapsed: {
        events: true,
        "fc-1": true,
        "fc-future.v2": true,
      },
    });

    expect(loadTrackPrefs()).toEqual({
      trackOrder: ["events", "cpu"],
      collapsed: { events: true },
    });
  });

  it("filters dynamic chart references from an older stored blob on hydrate", () => {
    const ls = fakeLocalStorage();
    ls.setItem(
      TRACK_PREFS_STORAGE_KEY,
      JSON.stringify({
        trackOrder: ["fc-old", "fc-future.v2", "queue"],
        collapsed: {
          "fc-old": true,
          "fc-future.v2": true,
          queue: true,
        },
      }),
    );
    vi.stubGlobal("localStorage", ls);
    const store = createViewerStore({ scheduler: () => {} });
    hydrateTrackPrefs(store);

    expect(uiPrefs(store).trackOrder).toEqual(["queue"]);
    expect(uiPrefs(store).collapsed).toEqual({ queue: true });
  });

  it("keeps the store default when no lanes height was stored", () => {
    const ls = fakeLocalStorage();
    // A pref blob from before lanesHeight existed: order/collapse only.
    ls.setItem(
      TRACK_PREFS_STORAGE_KEY,
      JSON.stringify({ trackOrder: ["cpu"], collapsed: {} }),
    );
    vi.stubGlobal("localStorage", ls);
    expect(loadTrackPrefs()?.lanesHeight).toBeUndefined();
    const store = createViewerStore({ scheduler: () => {} });
    hydrateTrackPrefs(store);
    expect(uiPrefs(store).lanesViewportHeight).toBe(360); // untouched default
  });

  it("rejects a non-positive stored lanes height", () => {
    const ls = fakeLocalStorage();
    ls.setItem(
      TRACK_PREFS_STORAGE_KEY,
      JSON.stringify({ trackOrder: [], collapsed: {}, lanesHeight: 0 }),
    );
    vi.stubGlobal("localStorage", ls);
    expect(loadTrackPrefs()?.lanesHeight).toBeUndefined();
  });

  it("persists + restores the rail width across a fresh store", () => {
    vi.stubGlobal("localStorage", fakeLocalStorage());
    const s1 = manualScheduler();
    const dispose = mountTrackPrefsPersistence(s1.store);
    s1.store.update("uiPrefs", { railWidth: 420 });
    s1.flush(); // subscriber writes to localStorage
    dispose();

    const s2 = createViewerStore({ scheduler: () => {} });
    expect(uiPrefs(s2).railWidth).toBe(300); // store default before hydrate
    hydrateTrackPrefs(s2);
    expect(uiPrefs(s2).railWidth).toBe(420);
  });

  it("keeps the store default when no rail width was stored", () => {
    const ls = fakeLocalStorage();
    // A pref blob from before railWidth existed: order/collapse only.
    ls.setItem(
      TRACK_PREFS_STORAGE_KEY,
      JSON.stringify({ trackOrder: ["cpu"], collapsed: {} }),
    );
    vi.stubGlobal("localStorage", ls);
    expect(loadTrackPrefs()?.railWidth).toBeUndefined();
    const store = createViewerStore({ scheduler: () => {} });
    hydrateTrackPrefs(store);
    expect(uiPrefs(store).railWidth).toBe(300); // untouched default
  });

  it("rejects a non-positive stored rail width", () => {
    const ls = fakeLocalStorage();
    ls.setItem(
      TRACK_PREFS_STORAGE_KEY,
      JSON.stringify({ trackOrder: [], collapsed: {}, railWidth: 0 }),
    );
    vi.stubGlobal("localStorage", ls);
    expect(loadTrackPrefs()?.railWidth).toBeUndefined();
  });

  it("persists + restores the tasks-table column widths across a fresh store", () => {
    vi.stubGlobal("localStorage", fakeLocalStorage());
    const s1 = manualScheduler();
    const dispose = mountTrackPrefsPersistence(s1.store);
    s1.store.update("uiPrefs", { taskColWidths: { loc: 260, polls: 48 } });
    s1.flush(); // subscriber writes to localStorage
    dispose();

    const s2 = createViewerStore({ scheduler: () => {} });
    expect(uiPrefs(s2).taskColWidths).toEqual({}); // fresh default before hydrate
    hydrateTrackPrefs(s2);
    expect(uiPrefs(s2).taskColWidths).toEqual({ loc: 260, polls: 48 });
  });

  it("drops non-positive and non-numeric stored column widths", () => {
    const ls = fakeLocalStorage();
    ls.setItem(
      TRACK_PREFS_STORAGE_KEY,
      JSON.stringify({
        trackOrder: [],
        collapsed: {},
        taskColWidths: { loc: 260, polls: 0, total: "wide", longest: -5 },
      }),
    );
    vi.stubGlobal("localStorage", ls);
    expect(loadTrackPrefs()?.taskColWidths).toEqual({ loc: 260 });
  });

  it("persists + restores the issues-table column widths across a fresh store", () => {
    vi.stubGlobal("localStorage", fakeLocalStorage());
    const s1 = manualScheduler();
    const dispose = mountTrackPrefsPersistence(s1.store);
    s1.store.update("uiPrefs", { issueColWidths: { dot: 14, kind: 120 } });
    s1.flush(); // subscriber writes to localStorage
    dispose();

    const s2 = createViewerStore({ scheduler: () => {} });
    expect(uiPrefs(s2).issueColWidths).toEqual({}); // fresh default before hydrate
    hydrateTrackPrefs(s2);
    expect(uiPrefs(s2).issueColWidths).toEqual({ dot: 14, kind: 120 });
  });

  it("keeps the store default when no column widths were stored", () => {
    const ls = fakeLocalStorage();
    // A pref blob from before taskColWidths existed: order/collapse only.
    ls.setItem(
      TRACK_PREFS_STORAGE_KEY,
      JSON.stringify({ trackOrder: ["cpu"], collapsed: {} }),
    );
    vi.stubGlobal("localStorage", ls);
    expect(loadTrackPrefs()?.taskColWidths).toBeUndefined();
    const store = createViewerStore({ scheduler: () => {} });
    hydrateTrackPrefs(store);
    expect(uiPrefs(store).taskColWidths).toEqual({}); // untouched default
  });
});

describe("collapse height + re-expand windowing obligation", () => {
  it("uses the label-only height when collapsed", () => {
    expect(COLLAPSED_TRACK_H).toBe(36);
  });

  it("keeps a collapsed track in the ordered list so re-expand restores it", () => {
    // Collapse is a HEIGHT override, not a hide: the track stays in the column
    // (label-only), so flipping it back re-paints it from current state.
    const collapsed = { spans: true };
    const ids = orderedTracks([]).map((t) => t.id);
    expect(ids).toContain("spans");
    expect(isCollapsed(collapsed, "spans")).toBe(true);
  });
});
