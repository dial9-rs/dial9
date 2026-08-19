// Unit tests for the per-page store. The frame scheduler is injected because
// Node has no requestAnimationFrame.

import { describe, it, expect, vi, afterEach } from "vitest";
import {
  createStore,
  assertInScheduledRender,
  devRenderAssertStats,
  type FrameScheduler,
  type ViewerStore,
} from "./store.js";
import type { StoreState } from "../types/state.js";

// A controllable requestAnimationFrame stand-in: collects scheduled
// callbacks; frame() runs exactly the callbacks scheduled so far (anything
// scheduled DURING a frame waits for the next one, like real RAF).
function fakeRaf(): { scheduler: FrameScheduler; frame: () => void; pending: () => number } {
  const queue: (() => void)[] = [];
  return {
    scheduler: (cb) => {
      queue.push(cb);
    },
    frame: () => {
      for (const cb of queue.splice(0)) cb();
    },
    pending: () => queue.length,
  };
}

// Minimal two-slice shape: keeps mechanics tests independent of the real
// viewer state. Composition with the full StoreState is proven separately.
interface TestState {
  a: { n: number; keep: string };
  b: { s: string };
}

function testState(): TestState {
  return { a: { n: 0, keep: "k" }, b: { s: "" } };
}

// Full initial viewer state matching the real slice shapes.
function initialViewerState(): StoreState {
  return {
    trace: { trace: null },
    viewport: { viewStart: 0, viewEnd: 0, minTs: 0, maxTs: 0 },
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
    },
    poi: { filter: "sched", sortKey: "duration", sortDir: "desc", index: -1, railTab: "issues", taskSort: "total", taskSortDir: "desc", taskIndex: -1 },
    uiPrefs: {
      panelCollapsed: { spans: false, events: false, cpu: false, queue: false },
      trackOrder: [],
      collapsed: {},
      collapsedRuntimes: {},
      collapsedRuntimeMetrics: {},
      sidebarWidth: 320,
      railWidth: 300,
      taskColWidths: {},
      issueColWidths: {},
      lanesViewportHeight: 200,
      lanesScrollTop: 0,
      selectedSpanNames: new Set(),
      selectedEventNames: new Set(),
      spanFilter: "",
      spanPctFilter: 0,
      timeMode: "rel",
      tz: "utc",
      stacksAsFlamegraph: false,
    },
    view: {
      fieldCharts: [],
      inspectorTab: "task",
      expandedPollGroups: new Set(),
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
    },
    transient: { mouseNs: null, hoverEventTs: null, drag: null, keyboardSelection: null, atCursor: null },
    segments: { segments: new Map() },
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.unstubAllEnvs();
});

describe("slice isolation", () => {
  it("notifies only subscribers whose slice set intersects the change", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    const onA = vi.fn();
    const onB = vi.fn();
    store.subscribe(["a"], onA);
    store.subscribe(["b"], onB);

    store.update("a", { n: 1 });
    raf.frame();

    expect(onA).toHaveBeenCalledTimes(1);
    expect(onB).not.toHaveBeenCalled();
  });

  it("patch-merges the target slice and leaves other slices untouched (by identity)", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    const a0 = store.getState().a;
    const b0 = store.getState().b;

    store.update("a", { n: 7 });

    const { a, b } = store.getState();
    expect(a).not.toBe(a0); // slice replaced wholesale
    expect(a).toEqual({ n: 7, keep: "k" }); // unpatched field preserved
    expect(b).toBe(b0); // untouched slice keeps identity
  });

  it("passes the frame's full changed-slice set to affected subscribers", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    const onA = vi.fn();
    store.subscribe(["a"], onA);

    store.update("a", { n: 1 });
    store.update("b", { s: "x" });
    raf.frame();

    expect(onA).toHaveBeenCalledWith(store.getState(), new Set(["a", "b"]));
  });
});

describe("no in-place slice mutation (audit finding 1)", () => {
  it("freezes initial and replaced slices in dev builds: in-place writes throw", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    const initial = store.getState();
    expect(() => {
      // @ts-expect-error slice fields are readonly through getState()
      initial.a.n = 99;
    }).toThrow(TypeError);

    store.update("a", { n: 1 });
    const replaced = store.getState();
    expect(() => {
      // @ts-expect-error slice fields stay readonly on the replaced slice
      replaced.a.n = 99;
    }).toThrow(TypeError);
    expect(store.getState().a.n).toBe(1); // neither write landed
  });

  it("does not freeze slices outside dev builds (release pays nothing)", () => {
    vi.stubEnv("DEV", false);
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    expect(Object.isFrozen(store.getState().a)).toBe(false);
    store.update("a", { n: 1 });
    expect(Object.isFrozen(store.getState().a)).toBe(false);
  });
});

describe("one notification per frame (fake RAF)", () => {
  it("coalesces many updates across slices into one frame callback and one run per subscriber", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    const onBoth = vi.fn();
    store.subscribe(["a", "b"], onBoth);

    store.update("a", { n: 1 });
    store.update("a", { n: 2 });
    store.update("b", { s: "x" });
    expect(raf.pending()).toBe(1); // one scheduled tick, not three
    expect(onBoth).not.toHaveBeenCalled(); // nothing before the frame

    raf.frame();
    expect(onBoth).toHaveBeenCalledTimes(1);
    expect(store.getState().a.n).toBe(2); // last patch wins at flush time

    raf.frame(); // idle frame: no changes -> no notifications
    expect(onBoth).toHaveBeenCalledTimes(1);
  });

  it("re-arms after a flush: a later update schedules a new tick", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    const onA = vi.fn();
    store.subscribe(["a"], onA);

    store.update("a", { n: 1 });
    raf.frame();
    store.update("a", { n: 2 });
    expect(raf.pending()).toBe(1);
    raf.frame();

    expect(onA).toHaveBeenCalledTimes(2);
  });

  it("defers updates made during a flush to the next frame (store-update ordering)", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    const onB = vi.fn();
    store.subscribe(["a"], () => {
      store.update("b", { s: "from-subscriber" });
    });
    store.subscribe(["b"], onB);

    store.update("a", { n: 1 });
    raf.frame();
    expect(onB).not.toHaveBeenCalled(); // not in the same tick
    expect(raf.pending()).toBe(1); // but a new tick was scheduled

    raf.frame();
    expect(onB).toHaveBeenCalledTimes(1);
  });

  it("unsubscribe stops notifications, including for a subscriber removed mid-flush", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    const late = vi.fn();
    let unsubLate: (() => void) | null = null;
    // Runs first (insertion order) and removes the later subscriber.
    store.subscribe(["a"], () => {
      if (unsubLate !== null) unsubLate();
    });
    unsubLate = store.subscribe(["a"], late);

    store.update("a", { n: 1 });
    raf.frame();
    expect(late).not.toHaveBeenCalled();

    store.update("a", { n: 2 });
    raf.frame();
    expect(late).not.toHaveBeenCalled();
  });

  it("uses requestAnimationFrame when no scheduler is injected", () => {
    const raf = fakeRaf();
    vi.stubGlobal("requestAnimationFrame", raf.scheduler);
    const store = createStore(testState());
    const onA = vi.fn();
    store.subscribe(["a"], onA);

    store.update("a", { n: 1 });
    expect(raf.pending()).toBe(1);
    raf.frame();
    expect(onA).toHaveBeenCalledTimes(1);
  });
});

describe("throwing subscriber does not drop the frame's changed set (audit finding 3)", () => {
  it("re-arms and retries delivery next frame, so later subscribers still hear the change", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    let boom = true; // throw only on the first delivery
    store.subscribe(["a"], () => {
      if (boom) {
        boom = false;
        throw new Error("subscriber boom");
      }
    });
    const onA = vi.fn();
    store.subscribe(["a"], onA);

    store.update("a", { n: 1 });
    expect(() => raf.frame()).toThrow(/subscriber boom/); // still loud
    expect(onA).not.toHaveBeenCalled(); // frame aborted before onA
    expect(raf.pending()).toBe(1); // but the changed set was re-armed

    raf.frame(); // delivery retried: onA finally hears about "a"
    expect(onA).toHaveBeenCalledTimes(1);
    expect(onA).toHaveBeenCalledWith(store.getState(), new Set(["a"]));
  });

  it("merges the restored changed set with updates made before the throw (no double-arm)", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    let boom = true;
    store.subscribe(["a"], () => {
      if (boom) {
        boom = false;
        store.update("b", { s: "pre-throw" }); // already re-armed the tick
        throw new Error("subscriber boom");
      }
    });
    const onBoth = vi.fn();
    store.subscribe(["a", "b"], onBoth);

    store.update("a", { n: 1 });
    expect(() => raf.frame()).toThrow(/subscriber boom/);
    expect(raf.pending()).toBe(1); // merged into the already-armed tick

    raf.frame();
    expect(onBoth).toHaveBeenCalledTimes(1);
    // Both the restored "a" and the mid-flush "b" arrive together.
    expect(onBoth).toHaveBeenCalledWith(store.getState(), new Set(["a", "b"]));
  });
});

describe("transient channel", () => {
  it("transient updates bypass full-notification subscribers (real StoreState)", () => {
    const raf = fakeRaf();
    // Composition check: Store<StoreState> via the ViewerStore alias.
    const store: ViewerStore = createStore(initialViewerState(), {
      scheduler: raf.scheduler,
    });
    const fullRender = vi.fn();
    const overlay = vi.fn();
    store.subscribe(["trace", "viewport", "selection"], fullRender);
    store.subscribe(["transient"], overlay);

    store.update("transient", { mouseNs: 1234 });
    raf.frame();
    expect(overlay).toHaveBeenCalledTimes(1);
    expect(fullRender).not.toHaveBeenCalled(); // never triggers full renders

    store.update("viewport", { viewStart: 10, viewEnd: 20 });
    raf.frame();
    expect(fullRender).toHaveBeenCalledTimes(1);
    expect(overlay).toHaveBeenCalledTimes(1); // and vice versa
  });
});

describe("update() rejects explicit undefined in patches (audit finding 2)", () => {
  it("a patch may omit a field but must not set a non-optional field to undefined", () => {
    const raf = fakeRaf();
    const store: ViewerStore = createStore(initialViewerState(), {
      scheduler: raf.scheduler,
    });
    // exactOptionalPropertyTypes (tsconfig.json): Object.assign would stamp
    // an explicit undefined into the field, so it must not typecheck. The
    // fix is type-level; these lines are never legal call sites.
    // @ts-expect-error viewStart is number: explicit undefined is rejected
    store.update("viewport", { viewStart: undefined });
    // @ts-expect-error spanFocus is SpanFocus | null, never undefined
    store.update("selection", { spanFocus: undefined });

    // Omission (the legal way to leave a field alone) still typechecks.
    store.update("viewport", { viewEnd: 50 });
    raf.frame();
    expect(store.getState().viewport.viewEnd).toBe(50);
  });
});

describe("derived caches", () => {
  it("computes once per dependency change and is invalidated only by its deps", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    // compute sees only the declared deps (Pick), not the full state.
    const compute = vi.fn((s: Pick<TestState, "a">) => s.a.n * 2);
    const doubled = store.derived(["a"], compute);

    expect(doubled()).toBe(0);
    expect(doubled()).toBe(0);
    expect(compute).toHaveBeenCalledTimes(1); // cache hit on repeat get

    store.update("b", { s: "x" }); // non-dep slice: no invalidation
    expect(doubled()).toBe(0);
    expect(compute).toHaveBeenCalledTimes(1);

    store.update("a", { n: 21 }); // dep slice replaced: invalidated
    expect(doubled()).toBe(42);
    expect(doubled()).toBe(42);
    expect(compute).toHaveBeenCalledTimes(2);
  });

  it("narrows compute's argument to the declared deps (type-level, audit finding 5)", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    // Invalidation watches ONLY the declared deps, so an undeclared-slice
    // read would return stale caches silently; the Pick-narrowed compute
    // signature makes that unrepresentable.
    // @ts-expect-error 'b' was not declared as a dep of this derived getter
    store.derived(["a"], (s) => s.b.s);
    const keepOfA = store.derived(["a"], (s) => s.a.keep);
    expect(keepOfA()).toBe("k");
  });
});

describe("N18 dev assertion hook", () => {
  it("passes inside a notification tick and counts the invocation", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    const before = devRenderAssertStats();
    let ran = false;
    store.subscribe(["a"], () => {
      assertInScheduledRender("lane render");
      ran = true;
    });

    store.update("a", { n: 1 });
    raf.frame();

    expect(ran).toBe(true);
    const after = devRenderAssertStats();
    expect(after.calls).toBe(before.calls + 1);
    expect(after.violations).toBe(before.violations);
  });

  it("throws outside the scheduler in dev builds and counts the violation", () => {
    const before = devRenderAssertStats();
    expect(() => assertInScheduledRender("wheel-zoom render")).toThrow(
      /wheel-zoom render ran outside the store scheduler/,
    );
    expect(() => assertInScheduledRender()).toThrow(/render function/); // default context
    const after = devRenderAssertStats();
    expect(after.calls).toBe(before.calls + 2);
    expect(after.violations).toBe(before.violations + 2);
  });

  it("is a no-op (no throw, no counting) outside dev builds", () => {
    vi.stubEnv("DEV", false);
    const before = devRenderAssertStats();
    expect(() => assertInScheduledRender("release render")).not.toThrow();
    expect(devRenderAssertStats()).toEqual(before);
  });

  it("closes the gate even when a subscriber throws (and does not swallow the error)", () => {
    const raf = fakeRaf();
    const store = createStore(testState(), { scheduler: raf.scheduler });
    store.subscribe(["a"], () => {
      throw new Error("subscriber boom");
    });

    store.update("a", { n: 1 });
    expect(() => raf.frame()).toThrow(/subscriber boom/);
    // Gate closed: an out-of-tick render is still flagged afterwards.
    expect(() => assertInScheduledRender()).toThrow(/\[dial9 N18\]/);
  });
});
