// Tooltip tests: placement is a pure function of the cursor, cached dimensions,
// and the viewport (it never reads the DOM), and the content model reproduces
// the lane tooltip field-for-field. Node-pure.

import { describe, it, expect } from "vitest";
import {
  placeTooltip,
  laneTooltipModel,
  type TooltipSegment,
  type TooltipRow,
} from "./tooltip.js";
import type { LaneHoverData } from "../canvas/lanes/index.js";

// ── Placement (pure, cached-dimension) ─────────────────

describe("placeTooltip (V1, pure over cached dims)", () => {
  const vp = { width: 1000, height: 800 };

  it("places to the right/below the cursor by default", () => {
    expect(placeTooltip({ clientX: 100, clientY: 100 }, { width: 200, height: 50 }, vp)).toEqual({
      left: 112, // clientX + 12
      top: 116, // clientY + 16
    });
  });

  it("clamps against the right edge using the cached width", () => {
    // clientX+12 = 962 would overflow; clamp to width - w - 8 = 792.
    expect(placeTooltip({ clientX: 950, clientY: 100 }, { width: 200, height: 50 }, vp).left).toBe(792);
  });

  it("flips ABOVE the cursor when it would spill off the bottom", () => {
    // clientY+16 = 796, +50 = 846 > 792 -> ty = clientY - h - 8 = 722.
    expect(placeTooltip({ clientX: 100, clientY: 780 }, { width: 200, height: 50 }, vp).top).toBe(722);
  });

  it("keeps an 8px minimum margin on both axes", () => {
    const p = placeTooltip({ clientX: -100, clientY: -100 }, { width: 200, height: 50 }, vp);
    expect(p.left).toBe(8);
    expect(p.top).toBe(8);
  });

  it("honours the tall-tooltip dy (130) at the lane call site", () => {
    expect(placeTooltip({ clientX: 100, clientY: 100 }, { width: 200, height: 50 }, vp, 130).top).toBe(230);
  });

  it("is a pure function of its args: the RESULT tracks the cached dims, and a "
     + "hover storm touches no DOM (F3 proxy - no element is even passed)", () => {
    // Different cached dims -> different clamp, proving placement reads the
    // passed dims, never the element (there is no element to measure here).
    const wide = placeTooltip({ clientX: 950, clientY: 100 }, { width: 400, height: 50 }, vp).left;
    const narrow = placeTooltip({ clientX: 950, clientY: 100 }, { width: 100, height: 50 }, vp).left;
    expect(wide).toBe(592); // 1000 - 400 - 8
    expect(narrow).toBe(892); // 1000 - 100 - 8
    // Deterministic across a storm of calls (no hidden state, no reads).
    for (let i = 0; i < 200; i++) {
      const p = placeTooltip({ clientX: i, clientY: i }, { width: 200, height: 50 }, vp);
      expect(p).toEqual(placeTooltip({ clientX: i, clientY: i }, { width: 200, height: 50 }, vp));
    }
  });
});

// ── content model (reproduces the lane tooltip) ───────────────

const fmt = { formatTs: (ns: number): string => `t${ns}` };

function seg(rows: TooltipRow[], label: string): TooltipSegment | undefined {
  for (const row of rows) for (const s of row) if (s.label === label) return s;
  return undefined;
}
function rowText(rows: TooltipRow[]): string[] {
  return rows.map((row) =>
    row.map((s) => [s.label, s.value, s.hint].filter((x) => x !== undefined).join(" ")).join(" · "),
  );
}

function pollingData(): LaneHoverData {
  return {
    workerId: 2,
    ns: 5_000,
    state: "polling",
    onCpuRatio: null,
    parkDurationNs: null,
    kernelSchedDelayNs: null,
    blockInPlace: null,
    poll: {
      durationNs: 4_000,
      openEnded: false,
      taskId: 0x2a,
      spawnLoc: "src/main.rs:10",
      cpuSampleCount: 3,
      schedSampleCount: 1,
      spanCount: 2,
      spanNames: { work: 2 },
    },
    globalQueue: 5,
    localQueue: 2,
    activeTaskCount: 7,
    span: { name: "req", durationNs: 1_000, fields: [["path", "/x"]], parentName: "root" },
    hasClickableStack: true,
  };
}

describe("laneTooltipModel (G16 content)", () => {
  it("polling: header, state, poll detail, queues, active tasks, span", () => {
    const rows = laneTooltipModel(pollingData(), fmt);
    const texts = rowText(rows);
    // Header worker + time (formatted via the injected clock formatter).
    expect(texts[0]).toContain("Worker 2");
    expect(texts[0]).toContain("Time t5000");
    expect(seg(rows, "State:")!.value).toBe("⚡ Polling");
    expect(seg(rows, "Task ID:")!.value).toBe("0x2a");
    expect(seg(rows, "Spawned at:")!.value).toBe("src/main.rs:10");
    const cpu = seg(rows, "CPU samples:")!;
    expect(cpu.value).toBe("🔥 3");
    expect(cpu.hint).toBe("(click to view)");
    expect(seg(rows, "Sched events:")!.value).toBe("⚠️ 1 blocking");
    const spans = seg(rows, "Spans:")!;
    expect(spans.value).toBe("2");
    expect(spans.hint).toBe("(work×2)");
    expect(seg(rows, "Global Q:")!.value).toBe("5");
    expect(seg(rows, "Local Q:")!.value).toBe("2");
    expect(seg(rows, "Est. Total:")!.value).toBe("7"); // 5 + 2
    expect(seg(rows, "Active Tasks:")!.value).toBe("7");
    expect(seg(rows, "Span:")!.value).toContain("req");
    expect(seg(rows, "Fields:")!.value).toBe("path=/x");
    expect(seg(rows, "Parent:")!.value).toBe("root");
  });

  it("open-ended poll tags the duration; missing queues render '-'", () => {
    const d = pollingData();
    d.poll!.openEnded = true;
    d.globalQueue = null;
    d.localQueue = null;
    const rows = laneTooltipModel(d, fmt);
    expect(seg(rows, "Poll duration:")!.value).toContain("(open-ended, block_in_place)");
    expect(seg(rows, "Global Q:")!.value).toBe("-");
    expect(seg(rows, "Local Q:")!.value).toBe("-");
    expect(seg(rows, "Est. Total:")!.value).toBe("0");
  });

  it("parked: state + duration inline, kernel sched delay row", () => {
    const d: LaneHoverData = {
      ...pollingData(),
      state: "parked",
      poll: null,
      parkDurationNs: 2_000,
      kernelSchedDelayNs: 500_000,
      onCpuRatio: null,
    };
    const rows = laneTooltipModel(d, fmt);
    expect(seg(rows, "State:")!.value).toContain("💤 Parked for");
    expect(seg(rows, "Kernel sched delay:")).toBeDefined();
  });

  it("block_in_place: handoff row wins over scheduling", () => {
    const d: LaneHoverData = {
      ...pollingData(),
      state: "block_in_place",
      poll: null,
      onCpuRatio: 0.9, // present, but the handoff must take precedence
      blockInPlace: { fromTid: 11, toTid: 22, durationNs: 3_000 },
    };
    const rows = laneTooltipModel(d, fmt);
    expect(seg(rows, "State:")!.value).toContain("⚠️ block_in_place for");
    expect(seg(rows, "Worker handed off:")!.value).toBe("tid 11 → 22");
    expect(seg(rows, "Scheduling:")).toBeUndefined();
  });

  it("active with on-CPU ratio: a Scheduling row (suppressed while parked)", () => {
    const active: LaneHoverData = {
      ...pollingData(),
      state: "active",
      poll: null,
      onCpuRatio: 0.97,
    };
    expect(seg(laneTooltipModel(active, fmt), "Scheduling:")!.value).toContain("🟢 97.0% on-CPU");

    const parked: LaneHoverData = { ...active, state: "parked", parkDurationNs: 1_000 };
    expect(seg(laneTooltipModel(parked, fmt), "Scheduling:")).toBeUndefined();
  });

  it("T17: a non-complete window adds an explicit truncation warning row", () => {
    const trunc = laneTooltipModel(pollingData(), { ...fmt, coverage: "truncated" });
    expect(seg(trunc, "Window:")!.value).toContain("partial data");
    const over = laneTooltipModel(pollingData(), { ...fmt, coverage: "oversized" });
    expect(seg(over, "Window:")!.value).toContain("oversized");
    // Complete (default) adds no window row.
    expect(seg(laneTooltipModel(pollingData(), fmt), "Window:")).toBeUndefined();
  });
});
