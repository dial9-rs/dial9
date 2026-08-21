// segments.ts pure-function tests: extents, need/prefetch sets, budget
// admission, eviction planning, the raw-gzip cache, and the boundary-poll
// extraction/stitching - the decision layer, exercised without any
// orchestrator. The orchestrator wiring is segments.window.test.ts.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "./load.js";
import {
  deriveSegmentExtents,
  mapExtentToMonotonic,
} from "./segments.js";
import {
  BUDGET_EVICTION_THRESHOLD_FRACTION,
  capToBudget,
  computeNeedSet,
  computePrefetchSet,
  evictionTriggerBytes,
  planEviction,
  extentDistance,
  extentsOverlap,
} from "./segment-budget.js";
import {
  computeSegmentEdgePolls,
  computeWindowBoundaryPolls,
  segmentInvariants,
} from "./segment-boundary-polls.js";
import { createRawByteCache } from "./raw-byte-cache.js";
import { EVENT_TYPES } from "../../../trace_parser.js";
import type { ParsedTrace, TraceEvent } from "../../../trace_parser.js";
import type { ListedSegment } from "./segments.js";
import type { SegmentEntry } from "../../types/state.js";
import type { TimeRange } from "../../types/trace.js";

// ── Fixtures ─────────────────────────────────────────────────────────────

// Default Hive-style layout.
const key = (epoch: number, index = 0): string =>
  `traces/version=1/date=2026-07-08/service=svc-a/time=1030/instance=host-1/boot=boot-1/${epoch}-${index}.bin.gz`;

const range = (startNs: number, endNs: number): TimeRange => ({ startNs, endNs });

/** N segments, 60s apart, each spanning its full 60s slot (epoch e0+60i). */
function makeSegments(n: number, sizeBytes = 10): ListedSegment[] {
  const e0 = 1_751_970_000;
  return Array.from({ length: n }, (_, i) => ({
    key: key(e0 + i * 60),
    sizeBytes,
    extent: range((e0 + i * 60) * 1e9, (e0 + (i + 1) * 60) * 1e9),
  }));
}

function mkEvent(
  eventType: number,
  timestamp: number,
  workerId: number,
  extra: Partial<TraceEvent> = {}
): TraceEvent {
  return {
    eventType,
    timestamp,
    workerId,
    localQueue: 0,
    globalQueue: 0,
    cpuTime: 0,
    schedWait: 0,
    taskId: 0,
    spawnLocId: null,
    spawnLoc: null,
    ...extra,
  };
}

const pollStart = (ts: number, w: number, taskId = 0): TraceEvent =>
  mkEvent(EVENT_TYPES.PollStart, ts, w, {
    taskId,
    spawnLocId: taskId > 0 ? `loc-${taskId}` : null,
    spawnLoc: taskId > 0 ? `src/task${taskId}.rs:1` : null,
  });
const pollEnd = (ts: number, w: number): TraceEvent =>
  mkEvent(EVENT_TYPES.PollEnd, ts, w);
const park = (ts: number, w: number): TraceEvent =>
  mkEvent(EVENT_TYPES.WorkerPark, ts, w);
const unpark = (ts: number, w: number): TraceEvent =>
  mkEvent(EVENT_TYPES.WorkerUnpark, ts, w);

// ── deriveSegmentExtents ─────────────────────────────────────────────────

describe("deriveSegmentExtents", () => {
  it("derives start from the filename epoch and end from last_modified", () => {
    const { segments, skipped } = deriveSegmentExtents([
      { key: key(1000), sizeBytes: 5, lastModifiedEpochS: 1070 },
    ]);
    expect(skipped).toEqual([]);
    expect(segments).toEqual([
      { key: key(1000), sizeBytes: 5, extent: range(1000e9, 1070e9) },
    ]);
  });

  it("floors a non-positive span to 1s (heatmap MIN_SEGMENT_SECONDS parity)", () => {
    const { segments } = deriveSegmentExtents([
      { key: key(1000), sizeBytes: 5, lastModifiedEpochS: 1000 },
    ]);
    expect(segments[0]!.extent).toEqual(range(1000e9, 1001e9));
  });

  it("tiles overlapping ends to the next start (upload lag, tileSegments parity)", () => {
    const { segments } = deriveSegmentExtents([
      { key: key(1060), sizeBytes: 5, lastModifiedEpochS: 1130 },
      { key: key(1000), sizeBytes: 5, lastModifiedEpochS: 1063 }, // overshoots next start
    ]);
    // Also sorts by start.
    expect(segments.map((s) => s.key)).toEqual([key(1000), key(1060)]);
    expect(segments[0]!.extent).toEqual(range(1000e9, 1060e9)); // clamped
    expect(segments[1]!.extent).toEqual(range(1060e9, 1130e9)); // untouched
  });

  it("leaves genuine gaps unclamped", () => {
    const { segments } = deriveSegmentExtents([
      { key: key(1000), sizeBytes: 5, lastModifiedEpochS: 1050 },
      { key: key(1100), sizeBytes: 5, lastModifiedEpochS: 1160 },
    ]);
    expect(segments[0]!.extent).toEqual(range(1000e9, 1050e9));
  });

  it("derives extents for unrecognized directory layouts via the basename (the dev-server's 6-component key)", () => {
    const demoKey = "traces/2026-04-09/1900/demo-service/local/host-0/abcd/1744225200-0.bin.gz";
    const { segments, skipped } = deriveSegmentExtents([
      { key: demoKey, sizeBytes: 5, lastModifiedEpochS: 1_744_225_260 },
    ]);
    expect(skipped).toEqual([]);
    expect(segments[0]!.extent.startNs).toBe(1_744_225_200e9);
  });

  it("skips keys with no filename epoch, with a reason - never guesses", () => {
    const { segments, skipped } = deriveSegmentExtents([
      { key: "custom/scheme/trace.bin", sizeBytes: 5, lastModifiedEpochS: 1000 },
    ]);
    expect(segments).toEqual([]);
    expect(skipped).toEqual([
      { key: "custom/scheme/trace.bin", reason: expect.stringContaining("epoch") as unknown },
    ]);
  });
});

describe("mapExtentToMonotonic", () => {
  it("subtracts the parse-derived clock offset (realtime - monotonic)", () => {
    expect(mapExtentToMonotonic(range(1000e9, 1060e9), 999e9)).toEqual(
      range(1e9, 61e9)
    );
  });
});

// ── need set / prefetch set ──────────────────────────────────────────────

describe("extent overlap and distance", () => {
  it("touching edges count as overlap (a poll at the edge belongs to both)", () => {
    expect(extentsOverlap(range(0, 10), range(10, 20))).toBe(true);
    expect(extentsOverlap(range(0, 10), range(11, 20))).toBe(false);
  });

  it("distance is the gap to the view, 0 when overlapping", () => {
    expect(extentDistance(range(0, 10), range(5, 15))).toBe(0);
    expect(extentDistance(range(0, 10), range(30, 40))).toBe(20);
    expect(extentDistance(range(50, 60), range(30, 40))).toBe(10);
  });
});

describe("computeNeedSet", () => {
  const segs = makeSegments(5);

  it("returns overlapping segments in listing order", () => {
    // A view spanning the middle of seg 1 to the middle of seg 3.
    const view = range(
      segs[1]!.extent.startNs + 30e9,
      segs[3]!.extent.startNs + 30e9
    );
    expect(computeNeedSet(segs, view)).toEqual([
      segs[1]!.key,
      segs[2]!.key,
      segs[3]!.key,
    ]);
  });

  it("is empty when the viewport misses every extent", () => {
    expect(computeNeedSet(segs, range(1, 2))).toEqual([]);
  });
});

describe("computePrefetchSet", () => {
  const segs = makeSegments(5);
  const viewOver = (i: number): TimeRange =>
    range(segs[i]!.extent.startNs + 10e9, segs[i]!.extent.startNs + 20e9);

  it("is +/-1 segment beyond both need-set edges", () => {
    const need = computeNeedSet(segs, viewOver(2));
    expect(computePrefetchSet(segs, need, viewOver(2))).toEqual([
      segs[1]!.key,
      segs[3]!.key,
    ]);
  });

  it("clips at the listing edges", () => {
    expect(computePrefetchSet(segs, [segs[0]!.key], viewOver(0))).toEqual([
      segs[1]!.key,
    ]);
    expect(computePrefetchSet(segs, [segs[4]!.key], viewOver(4))).toEqual([
      segs[3]!.key,
    ]);
  });

  it("prefetches the nearest neighbors when the need set is empty (coverage gap)", () => {
    const gappy: ListedSegment[] = [
      { key: "a", sizeBytes: 1, extent: range(0, 10) },
      { key: "b", sizeBytes: 1, extent: range(100, 110) },
    ];
    expect(computePrefetchSet(gappy, [], range(40, 50))).toEqual(["a", "b"]);
    // Beyond all data: only the nearest segment on the populated side.
    expect(computePrefetchSet(gappy, [], range(200, 210))).toEqual(["b"]);
  });
});

// ── budget admission ─────────────────────────────────────────────────────

describe("capToBudget", () => {
  const candidates = [
    { key: "far-left", extent: range(0, 10), estimatedRawBytes: 40 },
    { key: "center", extent: range(20, 30), estimatedRawBytes: 40 },
    { key: "right", extent: range(30, 40), estimatedRawBytes: 40 },
  ];
  const view = range(20, 30);

  it("admits nearest-to-center first, within capacity", () => {
    const plan = capToBudget(candidates, view, 100);
    expect(plan.admitted).toEqual(["center", "right"]);
    expect(plan.deferred).toEqual(["far-left"]);
  });

  it("always admits the nearest candidate of the first round, even oversized (window limit, not rejection)", () => {
    const plan = capToBudget(candidates, view, 10);
    expect(plan.admitted).toEqual(["center"]);
    expect(plan.deferred).toEqual(["right", "far-left"]);
  });

  it("never force-admits on later rounds (baseBytes > 0: the prefetch round)", () => {
    const plan = capToBudget(candidates, view, 100, 95);
    expect(plan.admitted).toEqual([]);
    expect(plan.deferred.length).toBe(3);
  });
});

// ── eviction planning ────────────────────────────────────────────────────

describe("planEviction", () => {
  const segs = makeSegments(5);
  const resident = (i: number, rawBytes: number) => ({
    key: segs[i]!.key,
    extent: segs[i]!.extent,
    rawBytes,
  });
  const view = range(segs[4]!.extent.startNs, segs[4]!.extent.endNs);
  const none = new Set<string>();

  it("does not trigger at or below the 90% threshold", () => {
    const plan = planEviction({
      resident: [resident(0, 450), resident(1, 450)],
      needKeys: none,
      prefetchKeys: none,
      view,
      budgetBytes: 1000,
    });
    // 900 === trigger exactly: not over, no eviction.
    expect(plan.triggered).toBe(false);
    expect(plan.evict).toEqual([]);
    expect(evictionTriggerBytes(1000)).toBe(900);
    expect(BUDGET_EVICTION_THRESHOLD_FRACTION).toBe(0.9);
  });

  it("fires past the threshold and evicts farthest-from-viewport first, down to the trigger", () => {
    const plan = planEviction({
      resident: [resident(0, 300), resident(1, 300), resident(3, 301)],
      needKeys: none,
      prefetchKeys: none,
      view,
      budgetBytes: 1000,
    });
    expect(plan.triggered).toBe(true);
    // 901 > 900: seg 0 is farthest from the view (over seg 4); dropping it
    // reaches 601 <= 900, so seg 1 (also over-distance) survives.
    expect(plan.evict).toEqual([segs[0]!.key]);
    expect(plan.residentBytesAfter).toBe(601);
  });

  it("never evicts need/prefetch members at the threshold stage", () => {
    const plan = planEviction({
      resident: [resident(0, 500), resident(1, 500)],
      needKeys: new Set([segs[0]!.key]),
      prefetchKeys: new Set([segs[1]!.key]),
      view,
      budgetBytes: 1000,
    });
    // 1000 > 900 but both are protected and the total is within the HARD
    // budget: nothing to do.
    expect(plan.triggered).toBe(true);
    expect(plan.evict).toEqual([]);
  });

  it("re-clamps under the HARD budget: prefetch first, then farthest need", () => {
    const plan = planEviction({
      resident: [resident(1, 500), resident(3, 500), resident(4, 500)],
      needKeys: new Set([segs[3]!.key, segs[4]!.key]),
      prefetchKeys: new Set([segs[1]!.key]),
      view,
      budgetBytes: 1000,
    });
    // 1500 > 1000: prefetch seg1 goes first (-> 1000, at the hard budget).
    expect(plan.evict).toEqual([segs[1]!.key]);
    expect(plan.residentBytesAfter).toBe(1000);

    const plan2 = planEviction({
      resident: [resident(3, 600), resident(4, 600)],
      needKeys: new Set([segs[3]!.key, segs[4]!.key]),
      prefetchKeys: none,
      view,
      budgetBytes: 1000,
    });
    // Only need members left: the farther-from-center one goes.
    expect(plan2.evict).toEqual([segs[3]!.key]);
    expect(plan2.residentBytesAfter).toBe(600);
  });

  it("reserves estimated bytes for admitted-but-unparsed work (frees room BEFORE the parse lands)", () => {
    const plan = planEviction({
      resident: [resident(0, 300), resident(1, 300), resident(2, 300)],
      needKeys: none,
      prefetchKeys: none,
      view,
      budgetBytes: 1000,
      reservedBytes: 300,
    });
    // 900 alone would sit exactly at the trigger; the 300-byte reservation
    // shrinks the effective trigger to 600, so the farthest resident goes
    // now instead of after the incoming parse overshoots.
    expect(plan.triggered).toBe(true);
    expect(plan.evict).toEqual([segs[0]!.key]);
    expect(plan.residentBytesAfter).toBe(600);
  });

  it("breaks distance ties deterministically (earlier extent first)", () => {
    const midView = range(segs[2]!.extent.startNs, segs[2]!.extent.endNs);
    const plan = planEviction({
      resident: [resident(1, 100), resident(3, 100), resident(2, 800)],
      needKeys: new Set([segs[2]!.key]),
      prefetchKeys: none,
      view: midView,
      budgetBytes: 1000,
    });
    // 1000 > 900. seg1 and seg3 are equidistant neighbors (both touch the
    // view); the earlier extent evicts first, and 900 <= trigger stops
    // there - seg3 survives.
    expect(plan.evict).toEqual([segs[1]!.key]);
    expect(plan.residentBytesAfter).toBe(900);
  });
});

// ── raw-gzip byte cache ──────────────────────────────────────────────────

describe("createRawByteCache", () => {
  const bytes = (n: number): Uint8Array => new Uint8Array(n);

  it("stores, retrieves, accounts and deletes", () => {
    const cache = createRawByteCache(100);
    expect(cache.set("a", bytes(30))).toBe(true);
    expect(cache.set("b", bytes(40))).toBe(true);
    expect(cache.totalBytes()).toBe(70);
    expect(cache.get("a")!.byteLength).toBe(30);
    expect(cache.get("missing")).toBeUndefined();
    expect(cache.delete("a")).toBe(true);
    expect(cache.totalBytes()).toBe(40);
    expect(cache.delete("a")).toBe(false);
  });

  it("evicts least-recently-used past 90% of its budget; get() refreshes recency", () => {
    const cache = createRawByteCache(100); // trigger 90
    cache.set("a", bytes(40));
    cache.set("b", bytes(40));
    cache.get("a"); // a is now more recent than b
    cache.set("c", bytes(40)); // 120 > 90: evict b (LRU), then a? 80 <= 90 stop
    expect(cache.keys()).toEqual(["a", "c"]);
    expect(cache.totalBytes()).toBe(80);
  });

  it("replacing a key updates accounting instead of double-counting", () => {
    const cache = createRawByteCache(100);
    cache.set("a", bytes(40));
    cache.set("a", bytes(50));
    expect(cache.totalBytes()).toBe(50);
    expect(cache.keys()).toEqual(["a"]);
  });

  it("rejects entries larger than the trigger (would evict the whole cache and still not fit)", () => {
    const cache = createRawByteCache(100);
    cache.set("a", bytes(40));
    expect(cache.set("big", bytes(91))).toBe(false);
    expect(cache.keys()).toEqual(["a"]);
    expect(cache.totalBytes()).toBe(40);
  });
});

// ── boundary polls: per-segment edge extraction ──────────────────────────

describe("computeSegmentEdgePolls", () => {
  it("captures a poll left open at the segment end, with its task metadata", () => {
    const edges = computeSegmentEdgePolls({
      events: [pollStart(10, 1, 7), pollEnd(20, 1), pollStart(30, 1, 8)],
    });
    expect(edges.openAtEnd).toEqual([
      {
        workerId: 1,
        start: 30,
        taskId: 8,
        spawnLocId: "loc-8",
        spawnLoc: "src/task8.rs:1",
      },
    ]);
    expect(edges.closeAtStart).toEqual([]);
  });

  it("a park-closed poll is NOT open at end (block_in_place is an in-segment openEnded case)", () => {
    const edges = computeSegmentEdgePolls({
      events: [pollStart(10, 1, 7), park(20, 1)],
    });
    expect(edges.openAtEnd).toEqual([]);
  });

  it("a PollStart-closed poll is NOT open at end; only the last open one is", () => {
    const edges = computeSegmentEdgePolls({
      events: [pollStart(10, 1, 7), pollStart(20, 1, 8)],
    });
    expect(edges.openAtEnd.length).toBe(1);
    expect(edges.openAtEnd[0]!.taskId).toBe(8);
  });

  it("captures a PollEnd-first segment start as a dangling close", () => {
    const edges = computeSegmentEdgePolls({
      events: [pollEnd(5, 2), pollStart(10, 2, 3), pollEnd(15, 2)],
    });
    expect(edges.closeAtStart).toEqual([{ workerId: 2, end: 5 }]);
    expect(edges.openAtEnd).toEqual([]);
  });

  it("a park- or unpark-first start is ambiguous and captured as nothing", () => {
    expect(
      computeSegmentEdgePolls({ events: [park(5, 1), pollEnd(6, 1)] })
        .closeAtStart
    ).toEqual([]);
    expect(
      computeSegmentEdgePolls({ events: [unpark(5, 1), pollEnd(6, 1)] })
        .closeAtStart
    ).toEqual([]);
  });

  it("handles multiple workers and unsorted event order", () => {
    const edges = computeSegmentEdgePolls({
      events: [
        pollStart(30, 2, 9), // open at end on worker 2
        pollEnd(5, 1), // dangling close on worker 1
        pollStart(10, 1, 4),
        pollEnd(20, 1),
      ],
    });
    expect(edges.closeAtStart).toEqual([{ workerId: 1, end: 5 }]);
    expect(edges.openAtEnd.map((o) => o.workerId)).toEqual([2]);
  });
});

describe("segmentInvariants", () => {
  it("carries min/max ts and the lifecycle-event worker set", () => {
    const inv = segmentInvariants({
      events: [
        pollStart(10, 3),
        pollEnd(20, 3),
        park(30, 1),
        mkEvent(EVENT_TYPES.QueueSample, 5, 0), // no worker attribution
      ],
      minTs: 5,
      maxTs: 30,
    });
    expect(inv).toEqual({ minTs: 5, maxTs: 30, workerIds: [1, 3] });
  });
});

// ── boundary polls: window-level stitching + truncation ──────────────────

/** A parsed entry over synthetic events (edge polls derived for real). */
function parsedEntry(extent: TimeRange, events: TraceEvent[]): SegmentEntry {
  const timestamps = events.map((e) => e.timestamp);
  const trace = {
    events,
    minTs: timestamps.length ? Math.min(...timestamps) : null,
    maxTs: timestamps.length ? Math.max(...timestamps) : null,
  } as unknown as ParsedTrace;
  return {
    state: "parsed",
    extent,
    sizeBytes: 1,
    trace,
    rawByteLength: 100,
    invariants: segmentInvariants(trace),
    edgePolls: computeSegmentEdgePolls(trace),
  };
}

const listedEntry = (extent: TimeRange): SegmentEntry => ({
  state: "listed",
  extent,
  sizeBytes: 1,
});

/**
 * A previously-parsed, now-evicted entry: the parse (trace) is gone, but
 * invariants AND edgePolls are retained across eviction - built from the
 * same synthetic events for realism.
 */
function evictedEntry(extent: TimeRange, events: TraceEvent[]): SegmentEntry {
  const { trace: _trace, ...kept } = parsedEntry(extent, events);
  return { ...kept, state: "evicted" };
}

describe("computeWindowBoundaryPolls", () => {
  const keys = ["s0", "s1", "s2", "s3"];
  const extents = [range(0, 100), range(100, 200), range(200, 300), range(300, 400)];

  it("stitches a poll across an internal resident boundary into a complete poll", () => {
    const entries = new Map<string, SegmentEntry>([
      // s1 ends mid-poll (task 7); s2 starts with the matching PollEnd.
      ["s0", listedEntry(extents[0]!)],
      ["s1", parsedEntry(extents[1]!, [pollStart(110, 1), pollEnd(120, 1), pollStart(190, 1, 7)])],
      ["s2", parsedEntry(extents[2]!, [pollEnd(210, 1), pollStart(250, 1), pollEnd(260, 1)])],
      ["s3", listedEntry(extents[3]!)],
    ]);
    const { stitched, truncated } = computeWindowBoundaryPolls(keys, entries);
    expect(stitched).toEqual([
      {
        workerId: 1,
        start: 190,
        end: 210,
        taskId: 7,
        spawnLocId: "loc-7",
        spawnLoc: "src/task7.rs:1",
      },
    ]);
    // The run's outer edges have no crossing evidence in this fixture:
    // s1 starts with a PollStart, s2 ends with a PollEnd.
    expect(truncated).toEqual([]);
  });

  it("marks a poll crossing the window's right edge as truncated, clamped to observed data - never long", () => {
    const entries = new Map<string, SegmentEntry>([
      ["s0", listedEntry(extents[0]!)],
      // s1 is the only parsed segment; its last poll (started at 190)
      // has its PollEnd in unfetched s2.
      ["s1", parsedEntry(extents[1]!, [pollStart(110, 1), pollEnd(120, 1), pollStart(190, 1, 7)])],
      ["s2", listedEntry(extents[2]!)],
      ["s3", listedEntry(extents[3]!)],
    ]);
    const { stitched, truncated } = computeWindowBoundaryPolls(keys, entries);
    expect(stitched).toEqual([]);
    expect(truncated).toEqual([
      {
        workerId: 1,
        start: 190,
        end: 190, // clamped to the segment's maxTs: a LOWER bound
        taskId: 7,
        spawnLocId: "loc-7",
        spawnLoc: "src/task7.rs:1",
        truncatedAt: "end",
        openEnded: true,
      },
    ]);
    // Never extended into the unfetched neighbor's extent.
    expect(truncated[0]!.end).toBeLessThanOrEqual(extents[1]!.endNs);
  });

  it("marks a poll crossing the window's left edge as truncated with an explicitly unknown task", () => {
    const entries = new Map<string, SegmentEntry>([
      ["s0", listedEntry(extents[0]!)],
      ["s1", parsedEntry(extents[1]!, [pollEnd(115, 2), pollStart(150, 2), pollEnd(160, 2)])],
      ["s2", listedEntry(extents[2]!)],
      ["s3", listedEntry(extents[3]!)],
    ]);
    const { truncated } = computeWindowBoundaryPolls(keys, entries);
    const left = truncated.filter((t) => t.truncatedAt === "start");
    expect(left).toEqual([
      {
        workerId: 2,
        start: 115, // clamped to the segment's minTs
        end: 115,
        taskId: null, // the PollStart (task identity) was never parsed
        spawnLocId: null,
        spawnLoc: null,
        truncatedAt: "start",
        openEnded: true,
      },
    ]);
  });

  it("keeps the frozen core's trace-edge behavior at the absolute listing ends (no marker)", () => {
    const entries = new Map<string, SegmentEntry>([
      // s0 (first listed) starts with a dangling close; s3 (last listed)
      // ends mid-poll. Neither has a LISTED neighbor beyond the edge, so
      // both keep core parity: dropped, not marked.
      ["s0", parsedEntry(extents[0]!, [pollEnd(10, 1), pollStart(20, 1), pollEnd(30, 1)])],
      ["s1", parsedEntry(extents[1]!, [pollStart(110, 1), pollEnd(120, 1)])],
      ["s2", parsedEntry(extents[2]!, [pollStart(210, 1), pollEnd(220, 1)])],
      ["s3", parsedEntry(extents[3]!, [pollStart(390, 1, 5)])],
    ]);
    const { stitched, truncated } = computeWindowBoundaryPolls(keys, entries);
    expect(truncated).toEqual([]);
    expect(stitched).toEqual([]);
  });

  it("drops unmatched evidence at internal boundaries (core #194 parity)", () => {
    const entries = new Map<string, SegmentEntry>([
      ["s0", listedEntry(extents[0]!)],
      // s1 ends mid-poll on worker 1, but s2's worker-1 stream begins
      // with a PollStart: the counterpart never made it to the wire.
      ["s1", parsedEntry(extents[1]!, [pollStart(190, 1, 7)])],
      ["s2", parsedEntry(extents[2]!, [pollStart(250, 1), pollEnd(260, 1)])],
      ["s3", listedEntry(extents[3]!)],
    ]);
    const { stitched, truncated } = computeWindowBoundaryPolls(keys, entries);
    expect(stitched).toEqual([]);
    expect(truncated).toEqual([]);
  });

  it("evicted segments end a run: their boundary becomes a window edge", () => {
    const entries = new Map<string, SegmentEntry>([
      ["s0", listedEntry(extents[0]!)],
      ["s1", parsedEntry(extents[1]!, [pollStart(190, 1, 7)])],
      // s2 was parsed and evicted: no longer resident.
      [
        "s2",
        {
          state: "evicted",
          extent: extents[2]!,
          sizeBytes: 1,
          rawByteLength: 100,
          invariants: { minTs: 210, maxTs: 260, workerIds: [1] },
        },
      ],
      ["s3", listedEntry(extents[3]!)],
    ]);
    const { truncated } = computeWindowBoundaryPolls(keys, entries);
    expect(truncated.map((t) => t.truncatedAt)).toEqual(["end"]);
    expect(truncated[0]!.taskId).toBe(7);
  });
});

// ── Polls spanning 3+ segments (N-segment chains) ────────────────────────
//
// A poll whose PollStart is in segment k and PollEnd in segment k+2, with the
// worker silent through k+1, must not vanish. Silence through a whole adjacent
// segment is itself evidence: any PollEnd/Park/PollStart there would have
// closed the poll, so the chain walk carries the open across silent segments.

describe("computeWindowBoundaryPolls: audit finding 1 (N-segment stitching)", () => {
  const keys3 = ["s0", "s1", "s2"];
  const extents3 = [range(0, 100), range(100, 200), range(200, 300)];

  it("stitches a poll spanning 3 resident segments (silent interior) into ONE complete poll - the audit probe", () => {
    const entries = new Map<string, SegmentEntry>([
      ["s0", parsedEntry(extents3[0]!, [pollStart(10, 1, 7)])],
      // Only worker-2 events: worker 1 is silent through all of s1.
      ["s1", parsedEntry(extents3[1]!, [pollStart(110, 2), pollEnd(120, 2)])],
      ["s2", parsedEntry(extents3[2]!, [pollEnd(210, 1), pollStart(250, 1), pollEnd(260, 1)])],
    ]);
    const { stitched, truncated } = computeWindowBoundaryPolls(keys3, entries);
    expect(stitched).toEqual([
      {
        workerId: 1,
        start: 10,
        end: 210,
        taskId: 7,
        spawnLocId: "loc-7",
        spawnLoc: "src/task7.rs:1",
      },
    ]);
    expect(truncated).toEqual([]);
  });

  it("stitches across TWO silent interior segments (4-segment span)", () => {
    const keys4 = ["s0", "s1", "s2", "s3"];
    const extents4 = [range(0, 100), range(100, 200), range(200, 300), range(300, 400)];
    const entries = new Map<string, SegmentEntry>([
      ["s0", parsedEntry(extents4[0]!, [pollStart(10, 1, 7)])],
      ["s1", parsedEntry(extents4[1]!, [pollStart(110, 2), pollEnd(120, 2)])],
      ["s2", parsedEntry(extents4[2]!, [pollStart(210, 3), pollEnd(220, 3)])],
      ["s3", parsedEntry(extents4[3]!, [pollEnd(310, 1)])],
    ]);
    const { stitched, truncated } = computeWindowBoundaryPolls(keys4, entries);
    expect(stitched).toEqual([
      {
        workerId: 1,
        start: 10,
        end: 310,
        taskId: 7,
        spawnLocId: "loc-7",
        spawnLoc: "src/task7.rs:1",
      },
    ]);
    expect(truncated).toEqual([]);
  });

  it("a prefix-resident chain surfaces as ONE poll truncated at the LAST resident edge", () => {
    const entries = new Map<string, SegmentEntry>([
      ["s0", parsedEntry(extents3[0]!, [pollStart(10, 1, 7)])],
      ["s1", parsedEntry(extents3[1]!, [pollStart(110, 2), pollEnd(190, 2)])],
      // The close lives in unfetched s2: the poll must still be visible
      // across ALL resident data it provably spans (s0's start through
      // s1's last observed event), truncated - never dropped as IDLE.
      ["s2", listedEntry(extents3[2]!)],
    ]);
    const { stitched, truncated } = computeWindowBoundaryPolls(keys3, entries);
    expect(stitched).toEqual([]);
    expect(truncated).toEqual([
      {
        workerId: 1,
        start: 10,
        end: 190, // s1's maxTs: the last RESIDENT observed event
        taskId: 7,
        spawnLocId: "loc-7",
        spawnLoc: "src/task7.rs:1",
        truncatedAt: "end",
        openEnded: true,
      },
    ]);
  });

  it("middle-only-resident: retained neighbor edge evidence surfaces the poll as both-edges-truncated with its task identity", () => {
    const entries = new Map<string, SegmentEntry>([
      // Both neighbors were parsed once and evicted; their edgePolls are
      // retained (eviction keeps them - they are tiny).
      ["s0", evictedEntry(extents3[0]!, [pollStart(10, 1, 7)])],
      ["s1", parsedEntry(extents3[1]!, [pollStart(110, 2), pollEnd(190, 2)])],
      ["s2", evictedEntry(extents3[2]!, [pollEnd(210, 1), pollStart(250, 1), pollEnd(260, 1)])],
    ]);
    const { stitched, truncated } = computeWindowBoundaryPolls(keys3, entries);
    // Not stitched (neither endpoint is resident); the resident middle
    // segment is provably mid-poll for worker 1 through its whole span.
    expect(stitched).toEqual([]);
    expect(truncated).toEqual([
      {
        workerId: 1,
        start: 110, // s1's minTs
        end: 190, // s1's maxTs
        taskId: 7, // known: the PollStart WAS parsed; evidence retained
        spawnLocId: "loc-7",
        spawnLoc: "src/task7.rs:1",
        truncatedAt: "both",
        openEnded: true,
      },
    ]);
  });

  it("a chain broken by an active segment with no matching close is dropped whole (core #194 parity)", () => {
    const entries = new Map<string, SegmentEntry>([
      ["s0", parsedEntry(extents3[0]!, [pollStart(10, 1, 7)])],
      ["s1", parsedEntry(extents3[1]!, [pollStart(110, 2), pollEnd(120, 2)])],
      // Worker 1 is ACTIVE here but its first event is a PollStart: the
      // close never made it to the wire - unmatched evidence drops, and
      // the silent-interior extension drops with it (the silence proof
      // is only as good as the wire).
      ["s2", parsedEntry(extents3[2]!, [pollStart(250, 1), pollEnd(260, 1)])],
    ]);
    const { stitched, truncated } = computeWindowBoundaryPolls(keys3, entries);
    expect(stitched).toEqual([]);
    expect(truncated).toEqual([]);
  });
});

// ── Extent adjacency gates stitching ─────────────────────────────────────
//
// The listing can have holes (retention-deleted objects, undecodable keys):
// two CONSECUTIVELY LISTED segments are not necessarily adjacent in time.
// Stitching across an extent gap would fabricate one completed long poll out
// of two different polls' evidence - a lie we must never tell.

describe("computeWindowBoundaryPolls: audit finding 3 (extent-gap guard)", () => {
  it("never stitches across an extent gap: both fragments surface truncated", () => {
    // Segment B (100..300) is missing from the listing; A and C are
    // consecutively listed but NOT adjacent in time.
    const entries = new Map<string, SegmentEntry>([
      ["A", parsedEntry(range(0, 100), [pollStart(10, 1, 7)])],
      ["C", parsedEntry(range(300, 400), [pollEnd(310, 1), pollStart(350, 1), pollEnd(360, 1)])],
    ]);
    const { stitched, truncated } = computeWindowBoundaryPolls(["A", "C"], entries);
    // The fabricated {start: 10, end: 310} completed poll must NOT exist.
    expect(stitched).toEqual([]);
    expect(truncated).toHaveLength(2);
    expect(truncated).toContainEqual({
      workerId: 1,
      start: 10,
      end: 10, // clamped to A's last observed event
      taskId: 7,
      spawnLocId: "loc-7",
      spawnLoc: "src/task7.rs:1",
      truncatedAt: "end",
      openEnded: true,
    });
    expect(truncated).toContainEqual({
      workerId: 1,
      start: 310, // clamped to C's first observed event
      end: 310,
      taskId: null, // unobserved gap: identity honestly unknown
      spawnLocId: null,
      spawnLoc: null,
      truncatedAt: "start",
      openEnded: true,
    });
  });

  it("touching or tiled-overlapping extents still count as adjacent (stitching preserved)", () => {
    const entries = new Map<string, SegmentEntry>([
      ["A", parsedEntry(range(0, 100), [pollStart(10, 1, 7)])],
      ["C", parsedEntry(range(100, 200), [pollEnd(110, 1)])],
    ]);
    const { stitched } = computeWindowBoundaryPolls(["A", "C"], entries);
    expect(stitched).toEqual([
      {
        workerId: 1,
        start: 10,
        end: 110,
        taskId: 7,
        spawnLocId: "loc-7",
        spawnLoc: "src/task7.rs:1",
      },
    ]);
  });
});

// ── real-demo anchor: the pure functions against a real parse ────────────

describe("boundary functions against the real demo trace", () => {
  let demo: ParsedTrace;

  beforeAll(async () => {
    const fileBytes = readFileSync(
      fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url))
    );
    const raw =
      fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
        ? new Uint8Array(gunzipSync(fileBytes))
        : new Uint8Array(fileBytes);
    demo = await parseTraceBuffer(raw);
  }, 60_000);

  it("segmentInvariants carries the parse bounds and a real worker set", () => {
    const inv = segmentInvariants(demo);
    expect(inv.minTs).toBe(demo.minTs);
    expect(inv.maxTs).toBe(demo.maxTs);
    expect(inv.workerIds.length).toBeGreaterThan(0);
    for (const w of inv.workerIds) expect(Number.isInteger(w)).toBe(true);
  });

  it("edge extraction stays within the parse bounds, at most one stub per worker per edge", () => {
    const edges = computeSegmentEdgePolls(demo);
    const seenEnd = new Set<number>();
    for (const open of edges.openAtEnd) {
      expect(seenEnd.has(open.workerId)).toBe(false);
      seenEnd.add(open.workerId);
      expect(open.start).toBeGreaterThanOrEqual(demo.minTs!);
      expect(open.start).toBeLessThanOrEqual(demo.maxTs!);
    }
    const seenStart = new Set<number>();
    for (const close of edges.closeAtStart) {
      expect(seenStart.has(close.workerId)).toBe(false);
      seenStart.add(close.workerId);
      expect(close.end).toBeGreaterThanOrEqual(demo.minTs!);
      expect(close.end).toBeLessThanOrEqual(demo.maxTs!);
    }
  });

  it("a two-copy window derives consistent truncation counts from the real edges", () => {
    const edges = computeSegmentEdgePolls(demo);
    const entry: SegmentEntry = {
      state: "parsed",
      extent: range(0, 100),
      sizeBytes: 1,
      trace: demo,
      rawByteLength: 1,
      invariants: segmentInvariants(demo),
      edgePolls: edges,
    };
    // [listed, parsed(demo), parsed(demo), listed]: both run edges are
    // window edges; the internal boundary stitches whatever matches.
    const entries = new Map<string, SegmentEntry>([
      ["a", { state: "listed", extent: range(0, 1), sizeBytes: 1 }],
      ["b", entry],
      ["c", { ...entry }],
      ["d", { state: "listed", extent: range(3, 4), sizeBytes: 1 }],
    ]);
    const { truncated, stitched } = computeWindowBoundaryPolls(
      ["a", "b", "c", "d"],
      entries
    );
    const starts = truncated.filter((t) => t.truncatedAt === "start");
    const ends = truncated.filter((t) => t.truncatedAt === "end");
    expect(starts.length).toBe(edges.closeAtStart.length);
    expect(ends.length).toBe(edges.openAtEnd.length);
    // Identical copies: every openAtEnd worker has a matching closeAtStart
    // only if that worker also starts with a dangling close.
    const closeWorkers = new Set(edges.closeAtStart.map((c) => c.workerId));
    const expectStitched = edges.openAtEnd.filter((o) =>
      closeWorkers.has(o.workerId)
    ).length;
    expect(stitched.length).toBe(expectStitched);
    for (const t of ends) {
      expect(t.end).toBeLessThanOrEqual(demo.maxTs!);
      expect(t.openEnded).toBe(true);
    }
  });
});
