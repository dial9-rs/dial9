// Store-integration tests for the spans track. The three checks that live at
// the store/render-input boundary (the browser-driven halves - row-walker, perf
// probe, visual - are covered elsewhere):
//   1. derived-cache invalidation: the trace-invariant span data recomputes
//      ONLY when the trace slice changes.
//   2. filter typing coalesces to <= 1 render/frame: the spans filter dispatches
//      a store update per keystroke and the RAF scheduler runs subscribers once
//      per frame - never a synchronous renderAll per keypress.
//   3. selection-driven dimming: drawSpansCanvas's render input reacts to the
//      selection slice - non-highlighted clusters recede.

import { describe, it, expect } from "vitest";
import { createStore } from "../../store/store.js";
import { createViewerStore, initialViewerState } from "./store.js";
import { computeSpanTrackData } from "./spans-model.js";
import type { SpanDrawBucket, SpanRenderModel, SpanTrackData } from "./spans-model.js";
import { drawSpansCanvas } from "./spans-track.js";
import type {
  CustomTraceEvent,
  ParsedTrace,
  TracingSpan,
} from "../../lib/trace/index.js";
import type { StoreState } from "../../types/state.js";

/** A manual frame scheduler: collect the pending flushes, run them on demand. */
function manualScheduler(): { pending: (() => void)[]; flush(): void } {
  const pending: (() => void)[] = [];
  return {
    pending,
    flush() {
      const cbs = pending.splice(0);
      for (const cb of cbs) cb();
    },
  };
}

function spanEvent(name: string, ts: number, spanId: string): CustomTraceEvent {
  return {
    name,
    timestamp: ts,
    fields: { worker_id: 0, span_id: spanId, span_name: "s" } as CustomTraceEvent["fields"],
    units: null,
  };
}

/** A ParsedTrace stub carrying only the customEvents the spans data reads. */
function fakeTrace(events: CustomTraceEvent[]): ParsedTrace {
  return { customEvents: events } as unknown as ParsedTrace;
}

// ── 1. Derived-cache invalidation ─────────────────────────────────────────

describe("span-data derived cache", () => {
  it("recomputes only when the trace slice changes", () => {
    const sched = manualScheduler();
    const store = createViewerStore({ scheduler: (cb) => sched.pending.push(cb) });
    let computes = 0;
    // The exact derived the component installs: keyed on the trace slice.
    const spanData = store.derived(["trace"], (s) => {
      computes++;
      return computeSpanTrackData(s.trace.trace?.customEvents);
    });

    const first = spanData();
    expect(computes).toBe(1);
    // Repeat call: memoized, no recompute.
    expect(spanData()).toBe(first);
    expect(computes).toBe(1);

    // A non-trace slice change must NOT invalidate the cache.
    store.update("viewport", { viewStart: 42 });
    store.update("uiPrefs", { spanFilter: "abc" });
    expect(spanData()).toBe(first);
    expect(computes).toBe(1);

    // Replacing the trace invalidates it.
    store.update("trace", {
      trace: fakeTrace([
        spanEvent("SpanEnter:x", 10, "a"),
        spanEvent("SpanExit:x", 20, "a"),
      ]),
    });
    const second = spanData();
    expect(computes).toBe(2);
    expect(second).not.toBe(first);
    expect(second.allSpans.map((s) => s.spanId)).toEqual(["a"]);
  });
});

// ── 2. Filter typing coalesces to <= 1 render/frame ───────────────────────

describe("filter typing coalescing", () => {
  it("collapses a burst of filter keystrokes into one render per frame", () => {
    const sched = manualScheduler();
    // A fresh store with the manual scheduler so we can count frames exactly.
    const store = createStore<StoreState>(initialViewerState(), {
      scheduler: (cb) => sched.pending.push(cb),
    });
    let renders = 0;
    // The shell subscribes to uiPrefs (among others); count its runs.
    store.subscribe(["uiPrefs"], () => {
      renders++;
    });

    // Five keystrokes = five store.update("uiPrefs", { spanFilter }) calls,
    // exactly what the spans filter input handler dispatches.
    for (const text of ["r", "re", "req", "requ", "reque"]) {
      store.update("uiPrefs", { spanFilter: text });
    }
    // Only ONE frame is scheduled for the whole burst.
    expect(sched.pending.length).toBe(1);
    expect(renders).toBe(0); // nothing has rendered yet

    sched.flush();
    // The subscriber ran exactly once for the five keystrokes (<= 1/frame).
    expect(renders).toBe(1);
    expect(store.getState().uiPrefs.spanFilter).toBe("reque");
  });
});

// ── 3. Selection-driven dimming render input ──────────────────────────────

/** A minimal recording 2d context: captures each fillRect's y + alpha. */
function recordingCtx(): {
  ctx: CanvasRenderingContext2D;
  fills: { y: number; alpha: number }[];
} {
  const fills: { y: number; alpha: number }[] = [];
  const ctx = {
    globalAlpha: 1,
    fillStyle: "",
    strokeStyle: "",
    font: "",
    textAlign: "left",
    clearRect() {},
    fillRect(_x: number, y: number, _w: number, _h: number) {
      fills.push({ y, alpha: (this as CanvasRenderingContext2D).globalAlpha });
    },
    fillText() {},
    beginPath() {},
    moveTo() {},
    lineTo() {},
    stroke() {},
    setTransform() {},
  } as unknown as CanvasRenderingContext2D;
  return { ctx, fills };
}

function bucket(id: string, y: number): SpanDrawBucket {
  const rep: TracingSpan = {
    start: 0,
    end: 100,
    spanId: id,
    spanName: `name-${id}`,
    fields: {},
    parentSpanId: null,
    segments: [{ start: 0, end: 100, workerId: 0 }],
    activeNs: 100,
    depth: 0,
    taskId: null,
  };
  return { spans: [rep], representative: rep, x1: 0, x2: 50, y, h: 8 };
}

function twoBucketModel(): SpanRenderModel {
  return {
    // minDur/maxDur 0 so the y-axis tick pass (its own alphas) is skipped.
    buckets: [bucket("A", 10), bucket("B", 20)],
    info: "",
    minDur: 0,
    maxDur: 0,
    renderCount: 2,
    emptyReason: null,
  };
}

const emptyData = computeSpanTrackData(null) as SpanTrackData;
const colorOf = (): string => "#ff0000";

function stateWithFocus(spanFocus: StoreState["selection"]["spanFocus"]): StoreState {
  const base = initialViewerState();
  return { ...base, selection: { ...base.selection, spanFocus } };
}

describe("drawSpansCanvas dimming", () => {
  it("draws every cluster at full weight when nothing is selected", () => {
    const { ctx, fills } = recordingCtx();
    drawSpansCanvas(ctx, twoBucketModel(), emptyData, stateWithFocus(null), 400, 120, 0, 400, colorOf);
    const a = fills.filter((f) => f.y === 10).map((f) => f.alpha);
    const b = fills.filter((f) => f.y === 20).map((f) => f.alpha);
    // envelope 0.15, active segment 0.8 - no dimming on either cluster.
    expect(a).toEqual([0.15, 0.8]);
    expect(b).toEqual([0.15, 0.8]);
  });

  it("dims non-selected clusters when a span is focused (render input)", () => {
    const { ctx, fills } = recordingCtx();
    const focus = { spanId: "A", chain: new Set(["A"]) };
    drawSpansCanvas(ctx, twoBucketModel(), emptyData, stateWithFocus(focus), 400, 120, 0, 400, colorOf);
    const a = fills.filter((f) => f.y === 10).map((f) => f.alpha);
    const b = fills.filter((f) => f.y === 20).map((f) => f.alpha);
    // Selected cluster A stays bright; non-selected B recedes to 0.08/0.3.
    expect(a).toEqual([0.4, 1.0]);
    expect(b).toEqual([0.08, 0.3]);
  });
});
