// Lane render/click/hover tests over a recording 2D-context stub and
// synthetic lane data: no DOM, no trace parse.

import { describe, it, expect } from "vitest";
import {
  renderLanes,
  sharedVisibleMaxQueue,
  type LaneDrawContext,
  type LanesRenderInput,
} from "./render.js";
import { resolveLaneClick } from "./click.js";
import { assembleLaneHover } from "./hover.js";
import type { PollSpan, TracingSpan, WorkerLane } from "../../../types/trace.js";
import type { TimePanelLayout } from "../../../types/state.js";

// ── Recording context ────────────────────────────────────────────────────

interface Recording {
  ctx: LaneDrawContext;
  fillRects: number;
  strokes: number;
  setLineDashes: number;
  fills: number;
  fillTexts: string[];
}

function recordingCtx(): Recording {
  const rec: Recording = {
    fillRects: 0,
    strokes: 0,
    setLineDashes: 0,
    fills: 0,
    fillTexts: [],
    ctx: null as unknown as LaneDrawContext,
  };
  rec.ctx = {
    fillStyle: "#000",
    strokeStyle: "#000",
    lineWidth: 1,
    font: "",
    textAlign: "left",
    clearRect: () => {},
    fillRect: () => {
      rec.fillRects++;
    },
    fillText: (t: string) => {
      rec.fillTexts.push(t);
    },
    beginPath: () => {},
    moveTo: () => {},
    lineTo: () => {},
    stroke: () => {
      rec.strokes++;
    },
    fill: () => {
      rec.fills++;
    },
    closePath: () => {},
    setLineDash: () => {
      rec.setLineDashes++;
    },
  };
  return rec;
}

// ── Synthetic layout + lane data ─────────────────────────────────────────

function layout(viewStart: number, viewEnd: number, drawW: number): TimePanelLayout {
  const labelW = 100;
  const span = viewEnd - viewStart || 1;
  return {
    pw: labelW + drawW,
    labelW,
    drawW,
    nsToPanelX: (ns) => labelW + ((ns - viewStart) / span) * drawW,
    nsToPanelXClamped: (ns) =>
      labelW + Math.max(0, Math.min(drawW, ((ns - viewStart) / span) * drawW)),
    panelXToNs: (x) => viewStart + ((x - labelW) / drawW) * span,
  };
}

function emptyLane(): WorkerLane {
  return { polls: [], parks: [], actives: [], cpuSampleTimes: [] };
}

function baseInput(over: Partial<LanesRenderInput>): LanesRenderInput {
  return {
    workerIds: [0],
    workerSpans: { 0: emptyLane() },
    workerQueueSamples: {},
    wakesByWorker: {},
    spansById: new Map(),
    blockInPlaceGaps: [],
    hasCpuTime: false,
    hasSchedWait: false,
    viewStart: 0,
    viewEnd: 1000,
    selectedTaskId: null,
    selectedSpanIds: new Set(),
    hoveredWakerTaskId: null,
    pinnedPoll: null,
    sharedMaxQ: 1,
    dimmer: (c) => c,
    ...over,
  };
}

function poll(start: number, end: number, taskId = 1): PollSpan {
  return { start, end, taskId, spawnLocId: null, spawnLoc: null };
}

// ── Downsample / coalesce usage ───────────────────────────

describe("renderLanes: pixel-bounded fills (downsample+coalesce)", () => {
  it("draws O(width) fillRects for a million polls, not O(polls)", () => {
    const drawW = 200;
    const n = 1_000_000;
    const polls: PollSpan[] = [];
    // Contiguous 1ns polls over [0, n): far more polls than pixel columns.
    for (let i = 0; i < n; i++) polls.push(poll(i, i + 1, (i % 7) + 1));
    const rec = recordingCtx();
    renderLanes(
      rec.ctx,
      baseInput({
        workerSpans: { 0: { ...emptyLane(), polls } },
        viewStart: 0,
        viewEnd: n,
      }),
      { time: layout(0, n, drawW), height: 60 },
    );
    // Poll band is downsampled to <= drawW representatives then coalesced, so
    // the total fillRects stay within a small multiple of the column count -
    // never anywhere near the poll count.
    expect(rec.fillRects).toBeLessThanOrEqual(drawW + 5);
    expect(rec.fillRects).toBeLessThan(n);
  });

  it("batches the local-queue step line to one stroke per lane, not per sample", () => {
    const drawW = 300;
    const n = 500_000;
    const samples: { t: number; local: number }[] = [];
    for (let i = 0; i < n; i++) samples.push({ t: i, local: (i % 11) + 1 });
    const rec = recordingCtx();
    renderLanes(
      rec.ctx,
      baseInput({
        workerQueueSamples: { 0: samples },
        viewStart: 0,
        viewEnd: n,
        sharedMaxQ: 11,
      }),
      { time: layout(0, n, drawW), height: 60 },
    );
    // Whole-canvas strokes: queue(1 style) + separator(1 style) = 2 stroke()
    // calls for one lane, independent of the 500k samples.
    expect(rec.strokes).toBeLessThanOrEqual(2);
    // The batcher hoists dash state; the solid queue/separator styles never
    // touch setLineDash on the draw path.
    expect(rec.setLineDashes).toBe(0);
    expect(rec.fillTexts.some((t) => t.startsWith("q:"))).toBe(true);
  });

  it("batches open-ended dashed markers: one dash-style stroke for many markers", () => {
    const drawW = 400;
    const polls: PollSpan[] = [];
    for (let i = 0; i < 10_000; i++) {
      const p = poll(i * 10, i * 10 + 5, 1);
      p.openEnded = true;
      polls.push(p);
    }
    const rec = recordingCtx();
    renderLanes(
      rec.ctx,
      baseInput({
        workerSpans: { 0: { ...emptyLane(), polls } },
        viewStart: 0,
        viewEnd: 100_000,
      }),
      { time: layout(0, 100_000, drawW), height: 60 },
    );
    // All open-ended markers of a lane drain in ONE dashed stroke; setLineDash
    // fires at most once for that style (then reset once) - not 10k times.
    expect(rec.setLineDashes).toBeLessThanOrEqual(2);
    expect(rec.strokes).toBeLessThanOrEqual(3); // openpoll + separator (+reset)
  });
});

describe("sharedVisibleMaxQueue", () => {
  it("is the max local depth over the visible window, min 1", () => {
    const s = {
      0: [
        { t: 0, local: 3 },
        { t: 100, local: 9 },
        { t: 200, local: 2 },
      ],
      1: [{ t: 150, local: 5 }],
    };
    // Window [90, 160] sees worker0's 9 (via <=/>= backup) and worker1's 5.
    expect(sharedVisibleMaxQueue([0, 1], s, 90, 160)).toBe(9);
    // Empty window still floors at 1.
    expect(sharedVisibleMaxQueue([0, 1], s, 1000, 2000)).toBeGreaterThanOrEqual(1);
  });
});

// ── Click resolution ───────────────────────────────────────

function span(
  spanId: string,
  start: number,
  end: number,
  workerId: number,
  parentSpanId: string | null = null,
): TracingSpan {
  return {
    start,
    end,
    spanId,
    spanName: `span-${spanId}`,
    fields: {},
    parentSpanId,
    segments: [{ start, end, workerId }],
    activeNs: end - start,
    depth: parentSpanId ? 1 : 0,
    taskId: null,
  };
}

describe("resolveLaneClick", () => {
  const polls = [poll(0, 100, 42), poll(200, 300, 7)];
  // Parent's segment ran on ANOTHER worker at this instant, so the
  // containing-span lookup on worker 0 finds the child; the ancestor walk
  // then re-adds the parent (its [start,end] still contains ns).
  const child = span("c", 10, 90, 0, "p");
  const parent = span("p", 0, 100, 1, null);
  const allSpans = [parent, child];
  const spanById = new Map(allSpans.map((s) => [s.spanId, s]));

  it("selects the poll's task and focuses the outermost containing span", () => {
    const r = resolveLaneClick({
      workerId: 0,
      ns: 50,
      polls,
      allSpans,
      spanById,
      currentSelectedTaskId: null,
    });
    expect(r.selectedTaskId).toBe(42);
    expect(r.focusedSpanId).toBe("p"); // outermost ancestor still containing 50
    expect(r.spanFocus?.chain.has("c")).toBe(true);
    expect(r.spanFocus?.chain.has("p")).toBe(true);
    expect(r.clearPinnedEvent).toBe(true);
    expect(r.toggledOff).toBe(false);
  });

  it("re-clicking the selected task toggles the whole selection off", () => {
    const r = resolveLaneClick({
      workerId: 0,
      ns: 50,
      polls,
      allSpans,
      spanById,
      currentSelectedTaskId: 42,
    });
    expect(r.selectedTaskId).toBeNull();
    expect(r.spanFocus).toBeNull();
    expect(r.focusedSpanId).toBeNull();
    expect(r.toggledOff).toBe(true);
  });

  it("opens Poll Detail only when the poll carries CPU or sched samples", () => {
    const withSamples = poll(0, 100, 42);
    withSamples.cpuSamples = [{ timestamp: 50 } as never];
    const r = resolveLaneClick({
      workerId: 0,
      ns: 50,
      polls: [withSamples],
      allSpans: [],
      spanById: new Map(),
      currentSelectedTaskId: null,
    });
    expect(r.openStackFor).toBe(withSamples);

    const bare = resolveLaneClick({
      workerId: 0,
      ns: 250,
      polls,
      allSpans: [],
      spanById: new Map(),
      currentSelectedTaskId: null,
    });
    expect(bare.openStackFor).toBeNull();
  });
});

// ── Hover assembly ─────────────────────────────────────────────────

describe("assembleLaneHover", () => {
  it("reports polling state with task, sample counts and clickable-stack", () => {
    const p = poll(0, 100, 0x1a);
    p.cpuSamples = [{ timestamp: 10 } as never, { timestamp: 20 } as never];
    p.spawnLoc = "src/main.rs:10";
    const lane: WorkerLane = { ...emptyLane(), polls: [p] };
    const data = assembleLaneHover({
      workerId: 2,
      ns: 50,
      spans: lane,
      allSpans: [],
      queueSamples: [{ t: 40, global: 3 }],
      localQueueSamples: [{ t: 40, local: 1 }],
      activeTaskSamples: [{ t: 0, count: 5 }],
      blockInPlaceGaps: [],
      hasCpuTime: true,
      hasSchedWait: true,
      hasTaskTracking: true,
    });
    expect(data.state).toBe("polling");
    expect(data.poll?.taskId).toBe(0x1a);
    expect(data.poll?.cpuSampleCount).toBe(2);
    expect(data.poll?.spawnLoc).toBe("src/main.rs:10");
    expect(data.hasClickableStack).toBe(true);
    expect(data.globalQueue).toBe(3);
    expect(data.localQueue).toBe(1);
    expect(data.activeTaskCount).toBe(5);
  });

  it("reports parked state with kernel sched delay", () => {
    const lane: WorkerLane = {
      ...emptyLane(),
      parks: [{ start: 0, end: 500, schedWait: 250 }],
    };
    const data = assembleLaneHover({
      workerId: 0,
      ns: 100,
      spans: lane,
      allSpans: [],
      queueSamples: [],
      localQueueSamples: [],
      activeTaskSamples: [],
      blockInPlaceGaps: [],
      hasCpuTime: false,
      hasSchedWait: true,
      hasTaskTracking: false,
    });
    expect(data.state).toBe("parked");
    expect(data.parkDurationNs).toBe(500);
    expect(data.kernelSchedDelayNs).toBe(250);
    expect(data.hasClickableStack).toBe(false);
  });
});
