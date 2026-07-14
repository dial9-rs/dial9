// Store-integration tests for the custom-events track. The store/render-input
// checks (the browser-driven halves - row-walker, visual - are listed in
// HANDOFF.md):
//   1. click-to-pin dispatches the selection slice: dispatchEventPin writes
//      selection.pinnedEvent with the right contract and toggles it off on a
//      repeat click, clearing any prior task/span selection.
//   2. hover dispatches the guide-line timestamp: dispatchHoverEvent writes
//      transient.hoverEventTs (the slice the overlay draws from) and clears it
//      on leave.
//   3. dim-on-selection: drawEventsCanvas's render input fades a cluster that
//      did not run on the highlighted task to 20%.
//   4. derived-cache invalidation: the trace-invariant event data recomputes
//      ONLY when the trace slice changes.

import { describe, it, expect } from "vitest";
import { createViewerStore } from "./store.js";
import {
  computeEventTrackData,
  type EventDrawBucket,
} from "./events-model.js";
import {
  dispatchEventPin,
  dispatchHoverEvent,
  drawEventsCanvas,
} from "./events-track.js";
import type { EventRenderModel } from "./events-model.js";
import type {
  CustomTraceEvent,
  ParsedTrace,
  PollSpan,
  WorkerLane,
} from "../../lib/trace/index.js";

function ev(name: string, timestamp: number): CustomTraceEvent {
  return { name, timestamp, fields: {} as CustomTraceEvent["fields"], units: null };
}

/** A viewer store with a no-op frame scheduler (node has no rAF); state +
 *  derived invalidation are synchronous, so dropped notifications are fine for
 *  the state assertions here. The one notification-timing test injects its own
 *  collecting scheduler. */
function testStore(): ReturnType<typeof createViewerStore> {
  return createViewerStore({ scheduler: () => {} });
}

function bucket(
  events: CustomTraceEvent[],
  taskId: number | null,
  px = 10,
): EventDrawBucket {
  return {
    events,
    representative: events[0]!,
    px,
    w: 3,
    y: 2,
    h: 36,
    taskId,
    baseAlpha: 0.55,
    color: "#aaa",
    secondColor: null,
    hitX: px - 6,
    hitW: 12,
  };
}

function poll(start: number, end: number, taskId: number): PollSpan {
  return { start, end, taskId, spawnLocId: null, spawnLoc: null };
}
function lane(polls: PollSpan[]): WorkerLane {
  return { polls, parks: [], actives: [], cpuSampleTimes: [] };
}

// ── 1. Click-to-pin dispatch ─────────────────────────────────────────────

describe("dispatchEventPin", () => {
  it("pins a single-event cluster into selection.pinnedEvent", () => {
    const store = testStore();
    // Seed a prior task/span selection so we can prove the pin clears them.
    store.update("selection", {
      selectedTaskId: 123,
      spanFocus: { spanId: "s", chain: new Set(["s"]) },
      focusedSpanId: "s",
    });

    const e = ev("db.query", 500);
    const lanes: Record<number, WorkerLane> = { 0: lane([poll(0, 1000, 42)]) };
    // The event has a worker_id, so resolvePollForEvent finds task 42's poll.
    (e.fields as Record<string, unknown>)["worker_id"] = 0;
    dispatchEventPin(store, bucket([e], 42), lanes, [0]);

    const pinned = store.getState().selection.pinnedEvent;
    expect(pinned).not.toBeNull();
    expect(pinned!.name).toBe("db.query");
    expect(pinned!.taskId).toBe(42);
    expect(pinned!.timestamp).toBe(500);
    expect(pinned!.detailEvent).toBe(e); // single event -> Related detail
    expect(pinned!.poll?.taskId).toBe(42);
    // Prior task/span selection dropped (legacy clearSpanTaskSelection).
    expect(store.getState().selection.selectedTaskId).toBeNull();
    expect(store.getState().selection.spanFocus).toBeNull();
    expect(store.getState().selection.focusedSpanId).toBeNull();
  });

  it("names a multi-event cluster and leaves detailEvent null", () => {
    const store = testStore();
    const events = [ev("a", 10), ev("b", 10)];
    dispatchEventPin(store, bucket(events, null), {}, []);
    const pinned = store.getState().selection.pinnedEvent!;
    expect(pinned.name).toBe("2 events");
    expect(pinned.taskId).toBeNull();
    expect(pinned.detailEvent).toBeNull(); // cluster -> no single detail
    expect(pinned.events).toHaveLength(2);
  });

  it("toggles the pin off when the same cluster is clicked again", () => {
    const store = testStore();
    const e = ev("x", 7);
    const b = bucket([e], null);
    dispatchEventPin(store, b, {}, []);
    expect(store.getState().selection.pinnedEvent).not.toBeNull();
    // A re-render rebuilds the bucket, but the representative event object is
    // the same -> recognized as the same cluster -> toggles off.
    dispatchEventPin(store, bucket([e], null), {}, []);
    expect(store.getState().selection.pinnedEvent).toBeNull();
  });

  it("switches the pin when a different cluster is clicked", () => {
    const store = testStore();
    const first = ev("x", 7);
    const second = ev("y", 9);
    dispatchEventPin(store, bucket([first], null), {}, []);
    dispatchEventPin(store, bucket([second], null), {}, []);
    expect(store.getState().selection.pinnedEvent!.events[0]).toBe(second);
  });
});

// ── 2. Hover guide-line dispatch ─────────────────────────────────────────

describe("dispatchHoverEvent", () => {
  it("writes and clears transient.hoverEventTs (the slice the crosshair draws from)", () => {
    const store = testStore();
    expect(store.getState().transient.hoverEventTs).toBeNull();
    dispatchHoverEvent(store, 4200);
    expect(store.getState().transient.hoverEventTs).toBe(4200);
    dispatchHoverEvent(store, null);
    expect(store.getState().transient.hoverEventTs).toBeNull();
  });

  it("does not re-dispatch when the timestamp is unchanged", () => {
    const pending: (() => void)[] = [];
    const store = createViewerStore({ scheduler: (cb) => pending.push(cb) });
    let notifications = 0;
    store.subscribe(["transient"], () => {
      notifications++;
    });
    dispatchHoverEvent(store, 100);
    dispatchHoverEvent(store, 100); // no-op: same ts -> no second frame
    // Only the first change scheduled a frame.
    expect(pending.length).toBe(1);
    for (const cb of pending.splice(0)) cb();
    expect(notifications).toBe(1);
  });
});

// ── 3. Dim-on-selection render input ─────────────────────────────────────

/** A minimal recording 2d context: captures each fillRect's x + alpha and
 *  every fillText string. */
function recordingCtx(): {
  ctx: CanvasRenderingContext2D;
  fills: { x: number; alpha: number }[];
  texts: string[];
} {
  const fills: { x: number; alpha: number }[] = [];
  const texts: string[] = [];
  const ctx = {
    globalAlpha: 1,
    fillStyle: "",
    font: "",
    textAlign: "left",
    clearRect() {},
    fillRect(x: number, _y: number, _w: number, _h: number) {
      fills.push({ x, alpha: (this as CanvasRenderingContext2D).globalAlpha });
    },
    fillText(text: string) {
      texts.push(text);
    },
    setTransform() {},
  } as unknown as CanvasRenderingContext2D;
  return { ctx, fills, texts };
}

function twoClusterModel(): EventRenderModel {
  // Cluster on task 1 at px 10, cluster on task 2 at px 50 (both single ticks).
  return {
    buckets: [bucket([ev("a", 10)], 1, 10), bucket([ev("b", 50)], 2, 50)],
    info: "2 events · 2 markers",
    emptyReason: null,
  };
}

describe("drawEventsCanvas dimming", () => {
  it("draws every tick at its base alpha when no task is highlighted", () => {
    const { ctx, fills } = recordingCtx();
    drawEventsCanvas(ctx, twoClusterModel(), null, 100, 40, false, false);
    // Both ticks at baseAlpha 0.55, no fade. (left = px - w/2 = px - 1.5.)
    expect(fills.find((f) => f.x === 8.5)!.alpha).toBeCloseTo(0.55, 10);
    expect(fills.find((f) => f.x === 48.5)!.alpha).toBeCloseTo(0.55, 10);
  });

  it("fades ticks not on the highlighted task to 20% (legacy)", () => {
    const { ctx, fills } = recordingCtx();
    // Highlight task 1: the px-10 cluster stays bright, the px-50 one dims.
    drawEventsCanvas(ctx, twoClusterModel(), 1, 100, 40, false, false);
    expect(fills.find((f) => f.x === 8.5)!.alpha).toBeCloseTo(0.55, 10); // on task 1
    expect(fills.find((f) => f.x === 48.5)!.alpha).toBeCloseTo(0.55 * 0.2, 10); // dimmed
  });

  it("paints the info readout on the canvas", () => {
    const { ctx, texts } = recordingCtx();
    drawEventsCanvas(ctx, twoClusterModel(), null, 100, 40, false, false);
    expect(texts).toContain("2 events · 2 markers");
  });

  it("paints the resting message and no info when there are no events", () => {
    const { ctx, texts } = recordingCtx();
    drawEventsCanvas(
      ctx,
      { buckets: [], info: "", emptyReason: "no-events" },
      null,
      100,
      40,
      false,
      false,
    );
    expect(texts).toContain("No custom events");
  });
});

// ── 4. Derived-cache invalidation ────────────────────────────────────────

/** A ParsedTrace stub carrying only the customEvents the event data reads. */
function fakeTrace(events: CustomTraceEvent[]): ParsedTrace {
  return { customEvents: events } as unknown as ParsedTrace;
}

describe("event-data derived cache", () => {
  it("recomputes only when the trace slice changes", () => {
    const store = testStore();
    let computes = 0;
    const eventData = store.derived(["trace"], (s) => {
      computes++;
      return computeEventTrackData(s.trace.trace?.customEvents);
    });

    const first = eventData();
    expect(computes).toBe(1);
    expect(eventData()).toBe(first);
    expect(computes).toBe(1);

    // Non-trace changes must NOT invalidate it.
    store.update("viewport", { viewStart: 42 });
    store.update("uiPrefs", { selectedEventNames: new Set(["x"]) });
    expect(eventData()).toBe(first);
    expect(computes).toBe(1);

    // Replacing the trace invalidates it.
    store.update("trace", { trace: fakeTrace([ev("real", 10)]) });
    const second = eventData();
    expect(computes).toBe(2);
    expect(second.events.map((e) => e.name)).toEqual(["real"]);
  });
});
