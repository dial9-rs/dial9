// Tests for the POI / issues-rail model.
//
// The headline check is rail-count parity: with the demo trace, the default
// filter ("sched") and worst-first, the derived rail lists EXACTLY the count
// the "x/74" stepper reported. We prove it by independently re-deriving the
// detector pipeline (buildWorkerSpans -> computeSchedulingDelays ->
// filterPointsOfInterest) and asserting the model's count equals it, for every
// detector filter - so a wrong default filter, a dropped worst-first, or a
// mismatched worker set would fail here.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import {
  EVENT_TYPES,
  attachCpuSamples,
  buildWorkerSpans,
  computeSchedulingDelays,
  filterPointsOfInterest,
  parseTraceBuffer,
} from "../../lib/trace/index.js";
import type {
  ParsedTrace,
  ParkSpan,
  PointOfInterest,
  PointOfInterestType,
  PollSpan,
} from "../../types/trace.js";
import type { PoiSlice, ViewportSlice } from "../../types/state.js";
import {
  POI_FILTERS,
  derivePoiViewModel,
  durationLabel,
  filterLabel,
  kindLabel,
  peakValue,
  poiJump,
  poisForFilter,
  poiSourceFor,
  relTimeLabel,
  severityOf,
  sortPois,
  stepIndex,
  valueNs,
  workerLabel,
} from "./poi.js";

const DEFAULT_POI: PoiSlice = {
  filter: "sched",
  sortKey: "duration",
  sortDir: "desc",
  index: -1,
};

let trace: ParsedTrace;

beforeAll(async () => {
  const fileBytes = readFileSync(
    fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)),
  );
  const raw =
    fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
      ? new Uint8Array(gunzipSync(fileBytes))
      : new Uint8Array(fileBytes);
  trace = await parseTraceBuffer(raw);
  expect(trace.minTs).not.toBeNull();
});

/** Re-derive the legacy detector count for one filter, from scratch. */
function legacyCount(t: ParsedTrace, filter: PointOfInterestType): number {
  const set = new Set<number>();
  for (const e of t.events) {
    if (e.eventType === EVENT_TYPES.QueueSample || e.eventType === EVENT_TYPES.WakeEvent) {
      continue;
    }
    set.add(e.workerId);
  }
  const workerIds = [...set].sort((a, b) => a - b);
  const spanResult = buildWorkerSpans(t.events, workerIds, t.maxTs ?? 0, t.blockInPlaceGaps);
  const workerSpans = spanResult.workerSpans;
  if (t.cpuSamples.length > 0) attachCpuSamples(t.cpuSamples, workerSpans);
  const schedDelays = computeSchedulingDelays(workerSpans, workerIds, spanResult.wakesByTask);
  return filterPointsOfInterest(filter, workerSpans, workerIds, schedDelays, {
    hasSchedWait: t.hasSchedWait,
    sortByWorst: true,
    taskInstrumented: t.taskInstrumented,
  }).length;
}

describe("rail count parity (DoD)", () => {
  it("lists exactly the legacy sched count at the default filter + worst-first", () => {
    const vm = derivePoiViewModel(trace, DEFAULT_POI, trace.minTs ?? 0);
    expect(vm.total).toBe(legacyCount(trace, "sched"));
    // The demo trace is expected to have scheduling delays (the "x/74" case).
    expect(vm.total).toBeGreaterThan(0);
  });

  it("matches the legacy count for every detector filter (all 5 wired)", () => {
    for (const filter of POI_FILTERS) {
      const vm = derivePoiViewModel(trace, { ...DEFAULT_POI, filter }, trace.minTs ?? 0);
      expect(vm.total, `filter=${filter}`).toBe(legacyCount(trace, filter));
    }
  });

  it("the displayed sort never changes the count (parity is sort-independent)", () => {
    const base = derivePoiViewModel(trace, DEFAULT_POI, trace.minTs ?? 0).total;
    for (const sortKey of ["worker", "kind", "time", "duration"] as const) {
      for (const sortDir of ["asc", "desc"] as const) {
        const vm = derivePoiViewModel(trace, { ...DEFAULT_POI, sortKey, sortDir }, trace.minTs ?? 0);
        expect(vm.rows.length).toBe(base);
      }
    }
  });
});

describe("poiSourceFor memoization", () => {
  it("returns the same source object for the same trace (buildWorkerSpans once)", () => {
    expect(poiSourceFor(trace)).toBe(poiSourceFor(trace));
  });

  it("memoizes the per-filter detector list", () => {
    const source = poiSourceFor(trace);
    expect(poisForFilter(source, "sched")).toBe(poisForFilter(source, "sched"));
  });
});

describe("red-flag counts", () => {
  it("drops zero-count detectors from the summary chip", () => {
    const vm = derivePoiViewModel(trace, DEFAULT_POI, trace.minTs ?? 0);
    expect(vm.redFlags.every((r) => r.count > 0)).toBe(true);
    // sched has matches on the demo trace, so it must appear.
    expect(vm.redFlags.some((r) => r.type === "sched")).toBe(true);
  });
});

// ── Pure-logic units (no trace needed) ───────────────────────────────────

function poll(start: number, end: number, taskId: number): PollSpan {
  return { start, end, taskId, spawnLocId: null, spawnLoc: null };
}
function park(start: number, end: number): ParkSpan {
  return { start, end };
}
function poi(
  type: PointOfInterestType,
  time: number,
  worker: number,
  value: number,
  span: PollSpan | ParkSpan,
  schedDelay?: PointOfInterest["schedDelay"],
): PointOfInterest {
  const base: PointOfInterest = { type, time, worker, value, span };
  return schedDelay ? { ...base, schedDelay } : base;
}

describe("sortPois", () => {
  const list: PointOfInterest[] = [
    poi("long-poll", 300, 1, 5, poll(300, 350, 10)),
    poi("long-poll", 100, 0, 20, poll(100, 200, 11)),
    poi("long-poll", 200, 2, 12, poll(200, 260, 12)),
  ];

  it("sorts by duration desc (worst-first) and asc", () => {
    expect(sortPois(list, "duration", "desc").map((p) => p.value)).toEqual([20, 12, 5]);
    expect(sortPois(list, "duration", "asc").map((p) => p.value)).toEqual([5, 12, 20]);
  });
  it("sorts by time and worker", () => {
    expect(sortPois(list, "time", "asc").map((p) => p.time)).toEqual([100, 200, 300]);
    expect(sortPois(list, "worker", "asc").map((p) => p.worker)).toEqual([0, 1, 2]);
  });
  it("does not mutate the input", () => {
    const before = list.map((p) => p.value);
    sortPois(list, "duration", "asc");
    expect(list.map((p) => p.value)).toEqual(before);
  });
});

describe("stepIndex (n/p semantics, legacy Prev/Next)", () => {
  it("lands on 0 from nothing-selected in either direction", () => {
    expect(stepIndex(5, -1, 1)).toBe(0);
    expect(stepIndex(5, -1, -1)).toBe(0);
  });
  it("steps and clamps at the ends", () => {
    expect(stepIndex(5, 2, 1)).toBe(3);
    expect(stepIndex(5, 4, 1)).toBe(4);
    expect(stepIndex(5, 0, -1)).toBe(0);
  });
  it("returns -1 for an empty list", () => {
    expect(stepIndex(0, -1, 1)).toBe(-1);
  });
});

describe("poiJump", () => {
  const vp: ViewportSlice = { viewStart: 0, viewEnd: 1e9, minTs: 0, maxTs: 1e9 };

  it("centers on a normal poll POI with 5x span / 30% left pad", () => {
    const p = poi("long-poll", 5e8, 1, 3, poll(5e8, 5e8 + 2e7, 7));
    const j = poiJump(p, vp);
    const viewDur = Math.max(2e7 * 5, 1e6);
    expect(j.viewStart).toBe(Math.max(0, 5e8 - viewDur * 0.3));
    expect(j.viewEnd).toBe(Math.min(1e9, j.viewStart + viewDur));
    expect(j.selectedTaskId).toBe(7); // poll POIs select their task
  });

  it("frames the wake->poll window and selects the delayed task", () => {
    const sd = {
      wakeTime: 4e8,
      pollTime: 4.5e8,
      delay: 5e7,
      taskId: 42,
      wakerTaskId: 1,
      worker: 0,
      poll: poll(4.5e8, 5e8, 42),
    };
    const p = poi("wake-delay", 4e8, 0, 500, sd.poll, sd);
    const j = poiJump(p, vp);
    expect(j.selectedTaskId).toBe(42);
    const padded = Math.max((5e8 - 4e8) * 3, 1e6);
    expect(j.viewStart).toBe(Math.max(0, 4e8 - padded * 0.2));
  });

  it("selects no task for a park (sched) POI", () => {
    const p = poi("sched", 1e8, 0, 84e6, park(1e8, 1e8 + 84e6));
    expect(poiJump(p, vp).selectedTaskId).toBeNull();
  });
});

describe("value + label formatting", () => {
  it("normalizes per-detector value units to ns", () => {
    expect(valueNs(poi("sched", 0, 0, 84e6, park(0, 1)))).toBe(84e6); // ns
    expect(valueNs(poi("long-poll", 0, 0, 5, poll(0, 1, 0)))).toBe(5e6); // ms->ns
    expect(valueNs(poi("wake-delay", 0, 0, 500, poll(0, 1, 0)))).toBe(500e3); // us->ns
  });
  it("labels worker / time columns in the mock format", () => {
    expect(workerLabel(1)).toBe("W1");
    expect(relTimeLabel(1.72e9, 0)).toBe("+1.72s");
  });
  it("formats the duration column as a human duration", () => {
    expect(durationLabel(poi("long-poll", 0, 0, 61, poll(0, 1, 0)))).toContain("ms");
  });
  it("gives every filter a full label and a short kind label", () => {
    for (const f of POI_FILTERS) {
      expect(filterLabel(f).length).toBeGreaterThan(0);
      expect(kindLabel(f).length).toBeGreaterThan(0);
    }
  });
});

describe("severity tiers", () => {
  it("tiers by fraction of the list peak", () => {
    const list = [
      poi("long-poll", 0, 0, 100, poll(0, 1, 0)),
      poi("long-poll", 0, 0, 50, poll(0, 1, 0)),
      poi("long-poll", 0, 0, 10, poll(0, 1, 0)),
    ];
    const peak = peakValue(list);
    expect(severityOf(list[0]!, peak)).toBe("high");
    expect(severityOf(list[1]!, peak)).toBe("med");
    expect(severityOf(list[2]!, peak)).toBe("low");
  });
  it("is low when the peak is zero", () => {
    expect(severityOf(poi("sched", 0, 0, 0, park(0, 1)), 0)).toBe("low");
  });
});
