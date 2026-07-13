// T06 DoD: exhaustive-switch probes over the app-level discriminated
// unions (src/types/trace.d.ts, src/types/state.d.ts).
//
// Placement: colocated under src/ (matched by the vitest include
// `src/**/*.test.ts`, vite.config.ts) rather than tests/, which is
// reserved for migrated legacy suites (tests/core/).
//
// Two jobs:
//
// 1. Compile-time: every switch below ends in a `default` branch that
//    assigns the discriminant to `never`. If a variant is ADDED to a
//    union and a switch doesn't handle it, the discriminant no longer
//    narrows to `never` in the default branch and `tsc --noEmit` fails.
//    Demonstration (verified during T06): temporarily adding
//      | { kind: "task-spawn"; timestamp: number }
//    to RuntimeEvent produced, in this file:
//      error TS2322: Type '"task-spawn"' is not assignable to type 'never'.
//    in BOTH eventTypeId() and describeEvent(). Same mechanism for
//    PanelKind and SegmentLifecycle.
//
// 2. Type-check anchor: tsconfig has `skipLibCheck: true`, so the .d.ts
//    bodies are only validated at USE sites. The typed sample values at
//    the bottom pin the slice shapes the way T05's probe.ts pins the
//    core declarations.

import { describe, expect, it } from "vitest";
import type {
  PollSpan,
  RuntimeEvent,
  RuntimeEventKind,
  TimeRange,
} from "./trace.js";
import type {
  PanelKind,
  SegmentLifecycle,
  StoreSliceName,
  StoreState,
} from "./state.js";

// ── Exhaustive switch over the event-kind union ─────────────────────────

/** Maps app-level kinds back to trace_parser.js EVENT_TYPES ids. */
function eventTypeId(kind: RuntimeEventKind): number {
  switch (kind) {
    case "poll-start":
      return 0;
    case "poll-end":
      return 1;
    case "worker-park":
      return 2;
    case "worker-unpark":
      return 3;
    case "queue-sample":
      return 4;
    case "wake":
      return 9;
    default: {
      // Compiles only while the cases above cover every RuntimeEventKind.
      const unhandled: never = kind;
      throw new Error(`unhandled event kind: ${String(unhandled)}`);
    }
  }
}

// ── Exhaustive switch over the event OBJECT union ───────────────────────
// Also proves per-variant narrowing: each branch touches a field that
// exists only on that variant, so a wrong discriminant fails to compile.

function describeEvent(ev: RuntimeEvent): string {
  switch (ev.kind) {
    case "poll-start":
      return `poll of task ${ev.taskId} (${ev.spawnLoc ?? "?"}) on worker ${ev.workerId}`;
    case "poll-end":
      return `poll end on worker ${ev.workerId}`;
    case "worker-park":
      // ADR-0002: tid is required-but-undefinable; consumers must branch.
      return `worker ${ev.workerId} parked${ev.tid !== undefined ? ` on tid ${ev.tid}` : ""}`;
    case "worker-unpark":
      return `worker ${ev.workerId} unparked after ${ev.schedWait}ns sched wait`;
    case "queue-sample":
      return `global queue depth ${ev.globalQueue}`;
    case "wake":
      return `task ${ev.wakerTaskId} woke task ${ev.wokenTaskId} onto worker ${ev.targetWorker}`;
    default: {
      const unhandled: never = ev;
      throw new Error(`unhandled event: ${String(unhandled)}`);
    }
  }
}

// ── Exhaustive switch over the panel union ──────────────────────────────

/** The DOM element id hosting each panel (viewer.html today). */
function panelElementId(panel: PanelKind): string {
  switch (panel) {
    case "spans":
      return "span-panel";
    case "events":
      return "custom-events-panel";
    case "cpu":
      return "cpu-panel";
    case "queue":
      return "queue-chart";
    case "task-detail":
      return "task-detail";
    default: {
      const unhandled: never = panel;
      throw new Error(`unhandled panel: ${String(unhandled)}`);
    }
  }
}

// ── Exhaustive switch over the segment lifecycle (architecture 2.8) ─────

function segmentHoldsParsedData(state: SegmentLifecycle): boolean {
  switch (state) {
    case "listed":
    case "fetching":
    case "evicted":
    // "oversized" (T17-audit finding 2): parsed once to learn the real
    // size, but the parse is never retained - tier-1 rendering only.
    case "oversized":
      return false;
    case "parsed":
      return true;
    default: {
      const unhandled: never = state;
      throw new Error(`unhandled segment state: ${String(unhandled)}`);
    }
  }
}

describe("app-level discriminated unions are exhaustively switchable", () => {
  it("maps every event kind to its EVENT_TYPES id", () => {
    expect(eventTypeId("poll-start")).toBe(0);
    expect(eventTypeId("poll-end")).toBe(1);
    expect(eventTypeId("worker-park")).toBe(2);
    expect(eventTypeId("worker-unpark")).toBe(3);
    expect(eventTypeId("queue-sample")).toBe(4);
    expect(eventTypeId("wake")).toBe(9);
  });

  it("narrows event variants to their own fields", () => {
    const park: RuntimeEvent = {
      kind: "worker-park",
      timestamp: 1_000,
      workerId: 2,
      localQueue: 0,
      cpuTime: 500,
      // ADR-0002: old traces predate tid; the field must still be
      // acknowledged explicitly -- omitting it is a compile error.
      tid: undefined,
    };
    expect(describeEvent(park)).toBe("worker 2 parked");

    const wake: RuntimeEvent = {
      kind: "wake",
      timestamp: 2_000,
      wakerTaskId: 7,
      wokenTaskId: 8,
      targetWorker: 1,
    };
    expect(describeEvent(wake)).toBe("task 7 woke task 8 onto worker 1");
  });

  it("maps every panel kind to its host element", () => {
    expect(panelElementId("spans")).toBe("span-panel");
    expect(panelElementId("events")).toBe("custom-events-panel");
    expect(panelElementId("cpu")).toBe("cpu-panel");
    expect(panelElementId("queue")).toBe("queue-chart");
    expect(panelElementId("task-detail")).toBe("task-detail");
  });

  it("knows which segment lifecycle states hold parsed data", () => {
    expect(segmentHoldsParsedData("listed")).toBe(false);
    expect(segmentHoldsParsedData("fetching")).toBe(false);
    expect(segmentHoldsParsedData("parsed")).toBe(true);
    expect(segmentHoldsParsedData("evicted")).toBe(false);
    expect(segmentHoldsParsedData("oversized")).toBe(false);
  });
});

// ── Slice-shape anchors (use-site checks; skipLibCheck bypasses .d.ts) ──

/** The page-load initial state: nothing loaded, nothing selected. */
const initialState: StoreState = {
  trace: { trace: null },
  viewport: { viewStart: 0, viewEnd: 0, minTs: 0, maxTs: 0 },
  selection: {
    selectedTaskId: null,
    spanFocus: null,
    focusedSpanId: null,
    pinnedEvent: null,
    sidebarRange: null,
    hoveredWakerTaskId: null,
  },
  uiPrefs: {
    // All four foldable panels start collapsed (features 02 O4);
    // Record<FoldablePanelKind, boolean> forces exactly these keys.
    panelCollapsed: { spans: true, events: true, cpu: true, queue: true },
    sidebarWidth: 400,
    selectedSpanNames: new Set(),
    selectedEventNames: new Set(),
    timeMode: "rel",
    tz: "utc",
  },
  transient: {
    mouseNs: null,
    hoverEventTs: null,
    drag: null,
    keyboardSelection: null,
  },
  segments: { segments: new Map() },
};

/** A mid-session state exercising the populated shapes. */
function populatedState(): StoreState {
  const poll: PollSpan = {
    start: 1_000,
    end: 5_000,
    taskId: 42,
    spawnLocId: "src/main.rs:10",
    spawnLoc: "src/main.rs:10",
  };
  const range: TimeRange = { startNs: 1_000, endNs: 9_000 };
  return {
    ...initialState,
    viewport: { viewStart: 1_000, viewEnd: 9_000, minTs: 0, maxTs: 20_000 },
    selection: {
      selectedTaskId: 42,
      spanFocus: { spanId: "s1", chain: new Set(["s1", "s0"]) },
      focusedSpanId: "s1",
      pinnedEvent: {
        events: [{ name: "MyEvent", timestamp: 2_000, fields: {}, units: null }],
        timestamp: 2_000,
        taskId: 42,
        name: "MyEvent",
        poll,
        detailEvent: null,
      },
      sidebarRange: range,
      hoveredWakerTaskId: 7,
    },
    transient: {
      mouseNs: 4_200,
      hoverEventTs: null,
      drag: { kind: "region-select", startX: 120, startNs: 2_000, moved: true },
      keyboardSelection: {
        kind: "zoom-select",
        startNs: 2_000,
        cursorNs: 3_000,
      },
    },
    segments: {
      segments: new Map([
        [
          "traces/2026-07-01/0900/host-a/0-0.bin.gz",
          { state: "parsed", extent: range, sizeBytes: 1_048_576 },
        ],
      ]),
    },
  };
}

describe("store slice shapes", () => {
  it("subscriber dependency sets are keyed by slice name", () => {
    const deps: ReadonlySet<StoreSliceName> = new Set([
      "viewport",
      "selection",
    ]);
    expect(deps.has("viewport")).toBe(true);
  });

  it("initial and populated states satisfy StoreState", () => {
    const state = populatedState();
    expect(initialState.trace.trace).toBeNull();
    expect(state.selection.selectedTaskId).toBe(42);
    expect(state.segments.segments.size).toBe(1);
    for (const [, entry] of state.segments.segments) {
      expect(segmentHoldsParsedData(entry.state)).toBe(true);
      expect(entry.extent.endNs).toBeGreaterThan(entry.extent.startNs);
    }
  });
});
