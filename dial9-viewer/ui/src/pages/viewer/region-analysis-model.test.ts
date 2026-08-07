// Region-analysis model behavioral tests. Node-testable pure derivations for
// the region-select flows: sample counts, heap estimates, blocking groups, the
// count reconciliation + frame->time extent, and the partial-window coverage.
// No DOM, no widget.

import { describe, expect, it } from "vitest";
import type {
  AllocEvent,
  CallframeSymbols,
  CpuSample,
  ParsedTrace,
  PollSpan,
  SymbolFrame,
  WorkerLane,
} from "../../lib/trace/index.js";
import type { SegmentsSlice } from "../../types/state.js";
import {
  buildBlockingView,
  collectSchedSamplesInRange,
  cpuCountLabel,
  cpuRegionView,
  defaultRegionMode,
  displayNamePath,
  filterRegionCpuSamples,
  frameSampleTimeExtent,
  heapRegionView,
  heapSamplesForMode,
  regionCoverage,
  regionModesPresent,
  regionTitle,
  type WorkerSpans,
} from "./region-analysis-model.js";

// ── fixtures ─────────────────────────────────────────────────────────────────

const OFF = 255;

function symbols(map: Record<string, string | SymbolFrame | SymbolFrame[]>): CallframeSymbols {
  const m: CallframeSymbols = new Map();
  for (const [addr, v] of Object.entries(map)) {
    if (typeof v === "string") m.set(addr, { symbol: v, location: null });
    else m.set(addr, v);
  }
  return m;
}

function cpu(
  timestamp: number,
  source: number,
  callchain: string[],
  workerId = 0,
): CpuSample {
  return { timestamp, source, callchain, workerId, tid: 1, cpu: null };
}

function alloc(timestamp: number, size: number, tid: number, callchain: string[]): AllocEvent {
  return { timestamp, size, tid, callchain, addr: "1" };
}

function poll(start: number, end: number, over: Partial<PollSpan> = {}): PollSpan {
  return { start, end, taskId: 1, spawnLocId: null, spawnLoc: null, ...over };
}

function lane(over: Partial<WorkerLane>): WorkerLane {
  return { polls: [], parks: [], actives: [], cpuSampleTimes: [], ...over };
}

function trace(over: Partial<ParsedTrace>): ParsedTrace {
  return {
    cpuSamples: [],
    allocEvents: [],
    callframeSymbols: new Map(),
    tidToWorker: new Map(),
    ...over,
  } as unknown as ParsedTrace;
}

// ── CPU region samples + count reconciliation ────────────────────────────────

describe("filterRegionCpuSamples", () => {
  it("keeps only on-CPU (source!=1) samples with a stack, honoring the range", () => {
    const samples = [
      cpu(10, 0, ["a"]), // foldable
      cpu(20, 1, ["a"]), // off-CPU (sched) - excluded
      cpu(30, 0, []), // stackless - excluded
      cpu(40, 0, ["a"]), // foldable but out of range
    ];
    expect(filterRegionCpuSamples(samples, 0, 35).map((s) => s.timestamp)).toEqual([10]);
    expect(filterRegionCpuSamples(samples, null, null).map((s) => s.timestamp)).toEqual([10, 40]);
  });
});

describe("cpuRegionView / cpuCountLabel (count reconciliation)", () => {
  it("separates foldable on-CPU samples from total CPU records", () => {
    // 4 records in range: 2 foldable on-CPU, 1 sched, 1 stackless.
    const t = trace({
      cpuSamples: [cpu(10, 0, ["a"]), cpu(12, 0, ["a"]), cpu(14, 1, ["a"]), cpu(16, 0, [])],
      callframeSymbols: symbols({ a: "leaf" }),
    });
    const view = cpuRegionView(t, { startNs: 0, endNs: 100 });
    expect(view.foldableCount).toBe(2);
    expect(view.totalRecords).toBe(4);
    expect(view.countLabel).toBe("2 samples (on-CPU, with stacks) of 4 CPU records");
  });

  it("drops the of-clause when every record is foldable (no contradiction)", () => {
    expect(cpuCountLabel(147, 147)).toBe("147 samples (on-CPU, with stacks)");
    // Toolbar-8993 total vs page-147 foldable, reconciled inline.
    expect(cpuCountLabel(147, 8993)).toBe(
      "147 samples (on-CPU, with stacks) of 8,993 CPU records",
    );
  });

  it("whole-trace view (null range) counts the full cpuSamples array", () => {
    const t = trace({ cpuSamples: [cpu(10, 0, ["a"]), cpu(20, 1, ["a"])] });
    const view = cpuRegionView(t, null);
    expect(view.totalRecords).toBe(2);
    expect(view.foldableCount).toBe(1);
  });
});

// ── region modes present + default ───────────────────────────────────────────

describe("regionModesPresent / defaultRegionMode", () => {
  const cs = symbols({ a: "leaf", s: "std::sync::Mutex::lock" });
  const ws: WorkerSpans = {
    0: lane({ polls: [poll(0, 100, { schedSamples: [cpu(50, 1, ["s"])] })] }),
  };

  it("detects cpu / blocking / heap presence in a range", () => {
    const t = trace({
      cpuSamples: [cpu(50, 0, ["a"])],
      allocEvents: [alloc(50, 4096, 1, ["a"])],
      callframeSymbols: cs,
      tidToWorker: new Map([[1, 0]]),
    });
    expect(regionModesPresent(t, ws, [0], { startNs: 0, endNs: 100 })).toEqual({
      cpu: true,
      blocking: true,
      heap: true,
    });
  });

  it("sched-only -> blocking; heap-only -> heap; else cpu", () => {
    expect(defaultRegionMode({ cpu: false, blocking: true, heap: false })).toBe("blocking");
    expect(defaultRegionMode({ cpu: false, blocking: false, heap: true })).toBe("heap");
    expect(defaultRegionMode({ cpu: false, blocking: true, heap: true })).toBe("heap");
    expect(defaultRegionMode({ cpu: true, blocking: true, heap: true })).toBe("cpu");
    expect(defaultRegionMode({ cpu: false, blocking: false, heap: false })).toBeNull();
  });
});

// ── heap ─────────────────────────────────────────────────────────────────────

describe("heapRegionView", () => {
  const cs = symbols({
    hook: "memory_profiling::hook::on_alloc",
    site: "my_crate::alloc_here",
  });

  it("strips hook frames, routes worker vs off-worker, sums HT estimates", () => {
    const t = trace({
      allocEvents: [
        alloc(10, 524288, 1, ["hook", "site"]), // tid 1 -> worker 0
        alloc(20, 1024, 9, ["site"]), // tid 9 -> unknown -> off-worker
        alloc(30, 4096, 1, []), // no stack after strip -> dropped (empty)
      ],
      callframeSymbols: cs,
      tidToWorker: new Map([[1, 0]]),
    });
    const view = heapRegionView(t, { startNs: 0, endNs: 100 }, {});
    expect(view.sampleCount).toBe(2);
    // first sample: hook stripped -> ["site"], worker 0
    expect(view.baseSamples[0]?.callchain).toEqual(["site"]);
    expect(view.baseSamples[0]?.workerId).toBe(0);
    expect(view.baseSamples[1]?.workerId).toBe(OFF);
    // size == R -> invP = 1/(1-e^-1) ~= 1.582; bytes ~= 524288 * that
    expect(view.baseSamples[0]?.byteWeight).toBeCloseTo(524288 / (1 - Math.exp(-1)), 0);
    expect(view.estimatedBytes).toBeGreaterThan(view.estimatedAllocs);
  });

  it("heapSamplesForMode swaps weight/allocWeight per Bytes/Count toggle", () => {
    const t = trace({
      allocEvents: [alloc(10, 1024, 1, ["site"])],
      callframeSymbols: cs,
      tidToWorker: new Map([[1, 0]]),
    });
    const base = heapRegionView(t, null, {}).baseSamples;
    const bytes = heapSamplesForMode(base, "bytes");
    const count = heapSamplesForMode(base, "count");
    expect(bytes[0]?.weight).toBe(base[0]?.byteWeight);
    expect(bytes[0]?.allocWeight).toBe(base[0]?.countWeight);
    expect(count[0]?.weight).toBe(base[0]?.countWeight);
    expect(count[0]?.allocWeight).toBe(base[0]?.byteWeight);
  });
});

// ── blocking-calls panel ─────────────────────────────────────────────────────

describe("buildBlockingView", () => {
  const cs = symbols({
    lock: "std::sync::Mutex::lock",
    epoll: "mio::epoll_wait",
    tokio: "tokio::runtime::park",
  });
  const ws: WorkerSpans = {
    0: lane({
      polls: [
        poll(0, 100, { schedSamples: [cpu(10, 1, ["lock", "tokio"]), cpu(20, 1, ["lock", "tokio"])] }),
        poll(200, 260, { schedSamples: [cpu(210, 1, ["epoll"])] }),
      ],
    }),
  };

  it("collects in-poll sched samples in range and groups by leaf", () => {
    const entries = collectSchedSamplesInRange(ws, [0], { startNs: 0, endNs: 300 });
    expect(entries).toHaveLength(3);
    const view = buildBlockingView(entries, cs, "leaf");
    expect(view.total).toBe(3);
    // lock group (2) sorts before epoll group (1)
    expect(view.groups[0]?.count).toBe(2);
    expect(view.groups[0]?.color).toBe("lock");
    expect(view.groups[0]?.pct).toBeCloseTo(66.7, 1);
    expect(view.groups[1]?.color).toBe("poll");
    // sub-stack marks the tokio framework frame
    const tokioFrame = view.groups[0]?.subStacks[0]?.frames.find((f) => f.isTokio);
    expect(tokioFrame).toBeTruthy();
  });

  it("range filtering excludes out-of-range polls/samples", () => {
    const entries = collectSchedSamplesInRange(ws, [0], { startNs: 0, endNs: 150 });
    expect(entries).toHaveLength(2); // only the first poll's two lock samples
    expect(buildBlockingView(entries, cs, "leaf").total).toBe(2);
  });

  it("group-by full splits distinct stacks; examples cap at 5 with a remainder", () => {
    const polls: PollSpan[] = [];
    for (let i = 0; i < 7; i++) {
      polls.push(poll(i * 10, i * 10 + 5, { schedSamples: [cpu(i * 10 + 1, 1, ["lock"])] }));
    }
    const many: WorkerSpans = { 0: lane({ polls }) };
    const entries = collectSchedSamplesInRange(many, [0], null);
    const view = buildBlockingView(entries, cs, "full");
    expect(view.groups[0]?.count).toBe(7);
    expect(view.groups[0]?.examples).toHaveLength(5);
    expect(view.groups[0]?.moreExamples).toBe(2);
  });
});

// ── frame -> timeline extent ─────────────────────────────────────────────────

describe("displayNamePath / frameSampleTimeExtent (frame->time)", () => {
  const cs = symbols({ leaf: "leaf_fn", mid: "mid_fn", root: "root_fn" });

  it("displayNamePath yields root-child-first display names (buildFlamegraphTree order)", () => {
    // callchain is leaf-first ["leaf","mid","root"]; the tree path is reversed.
    expect(displayNamePath(["leaf", "mid", "root"], cs)).toEqual(["root_fn", "mid_fn", "leaf_fn"]);
  });

  it("returns the [min,max] extent of samples flowing through the zoomed frame", () => {
    const samples = [
      { callchain: ["leaf", "mid", "root"], workerId: 0, timestamp: 100 },
      { callchain: ["leaf", "mid", "root"], workerId: 0, timestamp: 300 },
      { callchain: ["other", "root"], workerId: 0, timestamp: 500 }, // different mid
    ];
    // zoom into root_fn -> mid_fn (a two-frame prefix)
    const extent = frameSampleTimeExtent(
      samples,
      { worker: ["root_fn", "mid_fn"], offworker: [] },
      cs,
    );
    expect(extent).toEqual({ startNs: 100, endNs: 300 });
  });

  it("routes to the off-worker panel and returns null when nothing is zoomed", () => {
    const samples = [
      { callchain: ["leaf", "root"], workerId: 255, timestamp: 42 },
      { callchain: ["leaf", "root"], workerId: 0, timestamp: 99 },
    ];
    expect(
      frameSampleTimeExtent(samples, { worker: [], offworker: ["root_fn"] }, cs),
    ).toEqual({ startNs: 42, endNs: 42 });
    expect(frameSampleTimeExtent(samples, { worker: [], offworker: [] }, cs)).toBeNull();
  });
});

// ── partial-window coverage ──────────────────────────────────────────────────

describe("regionCoverage (partial-window badge)", () => {
  function segs(entries: { state: string; startNs: number; endNs: number }[]): SegmentsSlice {
    const m = new Map();
    entries.forEach((e, i) =>
      m.set(`seg${i}`, {
        state: e.state,
        extent: { startNs: e.startNs, endNs: e.endNs },
        sizeBytes: 0,
      }),
    );
    return { segments: m } as unknown as SegmentsSlice;
  }

  it("no segments (non-windowed load) is always complete", () => {
    expect(regionCoverage({ segments: new Map() }, { startNs: 0, endNs: 100 })).toBe("complete");
  });

  it("all overlapping segments parsed -> complete", () => {
    const s = segs([{ state: "parsed", startNs: 0, endNs: 100 }]);
    expect(regionCoverage(s, { startNs: 10, endNs: 90 })).toBe("complete");
  });

  it("an unparsed overlap -> partial; an oversized overlap -> oversized", () => {
    expect(
      regionCoverage(
        segs([
          { state: "parsed", startNs: 0, endNs: 100 },
          { state: "evicted", startNs: 100, endNs: 200 },
        ]),
        { startNs: 50, endNs: 150 },
      ),
    ).toBe("partial");
    expect(
      regionCoverage(
        segs([{ state: "oversized", startNs: 0, endNs: 100 }]),
        { startNs: 10, endNs: 90 },
      ),
    ).toBe("oversized");
  });

  it("ignores segments outside the region", () => {
    const s = segs([
      { state: "parsed", startNs: 0, endNs: 100 },
      { state: "evicted", startNs: 500, endNs: 600 },
    ]);
    expect(regionCoverage(s, { startNs: 10, endNs: 90 })).toBe("complete");
  });
});

describe("regionTitle (P3)", () => {
  it("formats a region duration or the whole trace", () => {
    expect(regionTitle({ startNs: 0, endNs: 2_500_000 })).toBe("2.50ms selected");
    expect(regionTitle(null)).toBe("Whole trace");
  });
});
