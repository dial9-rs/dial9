// createSegmentWindow orchestrator tests (T17): the wiring of the pure
// decision layer (segments.test.ts) to fetch/worker/store, driven with a
// mock fetcher, an injectable parser, a captured idle scheduler and a
// REAL T07 store (microtask scheduler, the T16 precedent). Every 2.8
// client-side hard edge is pinned here:
//
//   - stale-fetch abort on viewport jump (fetch phase AND parse phase);
//   - boundary polls marked truncated, never long;
//   - re-entry hits the raw cache, not the network (fetch call counts);
//   - eviction keeps resident raw under the budget throughout;
//
// plus the 10-segment end-to-end scenario (recorded-size accounting; the
// real-parse anchor test ties the accounting to actual buffers at demo
// scale - the accountant only ever does byteLength arithmetic, so
// recorded 30 MB entries behave identically to real 11 MB ones).

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync, gzipSync } from "node:zlib";
import { describe, expect, it, vi } from "vitest";
import { createStore } from "../../store/store.js";
import { EVENT_TYPES } from "../../../trace_parser.js";
import { parseTraceBuffer } from "./load.js";
import {
  RESIDENT_RAW_BUDGET_BYTES,
  createSegmentWindow,
  parseSegmentInWorker,
} from "./segments.js";
import type { Store, ViewerStore } from "../../store/store.js";
import type { ParsedTrace, TraceEvent } from "../../../trace_parser.js";
import type { SegmentEntry, SegmentsSlice } from "../../types/state.js";
import type { TimeRange } from "../../types/trace.js";
import type {
  ListedSegment,
  SegmentBytesFetcher,
  SegmentParseResult,
  SegmentParser,
  SegmentsSliceStore,
} from "./segments.js";
import type {
  TraceWorkerRequest,
  TraceWorkerResponse,
  TraceWorkerTiming,
} from "./worker/protocol.js";

// Compile-time: the full viewer store satisfies the orchestrator's store
// surface (fails tsc otherwise; the T16 TraceSliceStore precedent).
const _viewerStoreIsSegmentsSliceStore: (s: ViewerStore) => SegmentsSliceStore =
  (s) => s;
void _viewerStoreIsSegmentsSliceStore;

// ── Harness ──────────────────────────────────────────────────────────────

const MB = 1024 * 1024;
const range = (startNs: number, endNs: number): TimeRange => ({ startNs, endNs });

const segKey = (i: number): string =>
  `traces/2026-07-08/1030/svc/host/boot/${1_751_970_000 + i * 60}-0.bin.gz`;

/** N segments, 60s extents back-to-back. Keys double as URLs (urlFor id). */
function makeSegments(n: number, sizeBytes = 10): ListedSegment[] {
  return Array.from({ length: n }, (_, i) => ({
    key: segKey(i),
    sizeBytes,
    extent: range((1_751_970_000 + i * 60) * 1e9, (1_751_970_000 + (i + 1) * 60) * 1e9),
  }));
}

const viewOver = (segs: readonly ListedSegment[], i: number): TimeRange =>
  range(segs[i]!.extent.startNs + 10e9, segs[i]!.extent.startNs + 20e9);

type SegStore = Store<{ segments: SegmentsSlice }>;

function makeStore(): SegStore {
  return createStore<{ segments: SegmentsSlice }>(
    { segments: { segments: new Map() } },
    { scheduler: (cb) => queueMicrotask(cb) }
  );
}

const entryOf = (store: SegStore, key: string): SegmentEntry | undefined =>
  store.getState().segments.segments.get(key);

/** Drain microtasks + zero-delay timers (job chains have several awaits). */
const settle = async (): Promise<void> => {
  for (let i = 0; i < 4; i++) {
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
  }
};

interface PendingFetch {
  url: string;
  signal: AbortSignal | undefined;
  resolve(): void;
  reject(error: unknown): void;
}

/**
 * Mock fetcher: returns tagged buffers so the parser can tell which
 * segment it received (the byte content is otherwise irrelevant - the
 * parser is injected too). `manual` parks every call in `pending`.
 */
function makeFetcher(opts: { byteLength?: number; manual?: boolean } = {}) {
  const byteLength = opts.byteLength ?? 16;
  const tag = new WeakMap<ArrayBuffer, string>();
  const calls: string[] = [];
  const pending: PendingFetch[] = [];
  const bufferFor = (url: string): ArrayBuffer => {
    const buffer = new ArrayBuffer(byteLength);
    tag.set(buffer, url);
    return buffer;
  };
  const fetchBytes: SegmentBytesFetcher = (url, o) => {
    calls.push(url);
    if (!opts.manual) return Promise.resolve(bufferFor(url));
    return new Promise<ArrayBuffer>((resolve, reject) => {
      pending.push({
        url,
        signal: o.signal,
        resolve: () => {
          resolve(bufferFor(url));
        },
        reject,
      });
    });
  };
  return { fetchBytes, calls, pending, tag, bufferFor };
}

interface PendingParse {
  url: string | undefined;
  resolve(): void;
  reject(error: unknown): void;
}

/** Mock parser: resolves to a stub trace; `manual` parks jobs. */
function makeParser(opts: {
  tag: WeakMap<ArrayBuffer, string>;
  rawByteLength?: number;
  events?: (url: string | undefined) => TraceEvent[];
  manual?: boolean;
}) {
  const started: (string | undefined)[] = [];
  const aborted: (string | undefined)[] = [];
  const pending: PendingParse[] = [];
  const resultFor = (url: string | undefined): SegmentParseResult => {
    const events = opts.events?.(url) ?? [];
    const timestamps = events.map((e) => e.timestamp);
    const trace = {
      events,
      minTs: timestamps.length > 0 ? Math.min(...timestamps) : null,
      maxTs: timestamps.length > 0 ? Math.max(...timestamps) : null,
    } as unknown as ParsedTrace;
    return { trace, rawByteLength: opts.rawByteLength ?? 100 };
  };
  const parser: SegmentParser = (bytes) => {
    const url = opts.tag.get(bytes.buffer as ArrayBuffer);
    started.push(url);
    if (!opts.manual) {
      return {
        done: Promise.resolve(resultFor(url)),
        abort: () => {
          aborted.push(url);
        },
      };
    }
    let resolveDone!: (r: SegmentParseResult) => void;
    let rejectDone!: (e: unknown) => void;
    const done = new Promise<SegmentParseResult>((resolve, reject) => {
      resolveDone = resolve;
      rejectDone = reject;
    });
    done.catch(() => {});
    pending.push({
      url,
      resolve: () => {
        resolveDone(resultFor(url));
      },
      reject: rejectDone,
    });
    return {
      done,
      abort: () => {
        aborted.push(url);
      },
    };
  };
  return { parser, started, aborted, pending };
}

/** Captured idle scheduler: prefetch runs only when the test says so. */
function makeIdle() {
  const queue: (() => void)[] = [];
  return {
    idle: (cb: () => void): void => {
      queue.push(cb);
    },
    runIdles: (): void => {
      for (const cb of queue.splice(0)) cb();
    },
    queuedCount: () => queue.length,
  };
}

// Synthetic event builders (same shapes as segments.test.ts).
function mkEvent(eventType: number, timestamp: number, workerId: number, taskId = 0): TraceEvent {
  return {
    eventType,
    timestamp,
    workerId,
    localQueue: 0,
    globalQueue: 0,
    cpuTime: 0,
    schedWait: 0,
    taskId,
    spawnLocId: taskId > 0 ? `loc-${taskId}` : null,
    spawnLoc: taskId > 0 ? `src/task${taskId}.rs:1` : null,
  };
}
const pollStart = (ts: number, w: number, taskId = 0): TraceEvent =>
  mkEvent(EVENT_TYPES.PollStart, ts, w, taskId);
const pollEnd = (ts: number, w: number): TraceEvent =>
  mkEvent(EVENT_TYPES.PollEnd, ts, w);

// ── Basics ───────────────────────────────────────────────────────────────

describe("createSegmentWindow: basics", () => {
  it("seeds the slice with every segment listed, extents and sizes intact", () => {
    const segs = makeSegments(3);
    const store = makeStore();
    createSegmentWindow(store, segs, { idle: makeIdle().idle });
    for (const s of segs) {
      expect(entryOf(store, s.key)).toEqual({
        state: "listed",
        extent: s.extent,
        sizeBytes: s.sizeBytes,
      });
    }
  });

  it("viewport drives fetch -> parse -> parsed entries with parse-derived fields", async () => {
    const segs = makeSegments(4);
    const store = makeStore();
    const fetcher = makeFetcher();
    const parser = makeParser({
      tag: fetcher.tag,
      rawByteLength: 100,
      events: () => [pollStart(10, 1), pollEnd(20, 1)],
    });
    const idle = makeIdle();
    const win = createSegmentWindow(store, segs, {
      fetchBytes: fetcher.fetchBytes,
      parser: parser.parser,
      idle: idle.idle,
    });
    // A view spanning segments 1 and 2.
    win.setViewport(range(segs[1]!.extent.startNs + 1e9, segs[2]!.extent.startNs + 1e9));
    expect(entryOf(store, segKey(1))!.state).toBe("fetching");
    await settle();

    for (const i of [1, 2]) {
      const entry = entryOf(store, segKey(i))!;
      expect(entry.state).toBe("parsed");
      expect(entry.trace).toBeDefined();
      expect(entry.rawByteLength).toBe(100);
      expect(entry.invariants).toEqual({ minTs: 10, maxTs: 20, workerIds: [1] });
      expect(entry.edgePolls).toEqual({ openAtEnd: [], closeAtStart: [] });
    }
    // Only the need set fetched; prefetch (0 and 3) awaits idle.
    expect(fetcher.calls.sort()).toEqual([segKey(1), segKey(2)]);
    expect(win.stats().residentRawBytes).toBe(200);
    expect(win.stats().parsesStarted).toBe(2);
  });
});

// ── HARD EDGE: stale-fetch abort on viewport jump ────────────────────────

describe("hard edge: stale-fetch abort on viewport jump", () => {
  it("aborts an in-flight FETCH; a late resolution never reaches the parser or the store", async () => {
    const segs = makeSegments(10);
    const store = makeStore();
    const fetcher = makeFetcher({ manual: true });
    const parser = makeParser({ tag: fetcher.tag });
    const idle = makeIdle();
    const win = createSegmentWindow(store, segs, {
      fetchBytes: fetcher.fetchBytes,
      parser: parser.parser,
      idle: idle.idle,
    });

    win.setViewport(viewOver(segs, 0));
    expect(fetcher.pending.length).toBe(1);
    const seg0Fetch = fetcher.pending[0]!;
    expect(seg0Fetch.url).toBe(segKey(0));
    expect(seg0Fetch.signal?.aborted).toBe(false);
    expect(entryOf(store, segKey(0))!.state).toBe("fetching");

    // Viewport jumps far away: the stale fetch is aborted.
    win.setViewport(viewOver(segs, 8));
    expect(seg0Fetch.signal?.aborted).toBe(true);
    expect(entryOf(store, segKey(0))!.state).toBe("listed");
    expect(win.stats().abortedJobs).toBe(1);

    // The transport resolves late anyway (a fetch that ignored the
    // signal): the settled-job guard drops it - no parse, no store write.
    seg0Fetch.resolve();
    await settle();
    expect(parser.started).not.toContain(segKey(0));
    expect(entryOf(store, segKey(0))!.state).toBe("listed");
    expect(entryOf(store, segKey(0))!.trace).toBeUndefined();
  });

  it("aborts an in-flight PARSE; a late completion never mutates the store", async () => {
    const segs = makeSegments(10);
    const store = makeStore();
    const fetcher = makeFetcher();
    const parser = makeParser({ tag: fetcher.tag, manual: true });
    const idle = makeIdle();
    const win = createSegmentWindow(store, segs, {
      fetchBytes: fetcher.fetchBytes,
      parser: parser.parser,
      idle: idle.idle,
    });

    win.setViewport(viewOver(segs, 0));
    await settle();
    expect(parser.started).toEqual([segKey(0)]);
    expect(parser.pending.length).toBe(1);

    win.setViewport(viewOver(segs, 8));
    // The parse job was told to abort and the entry reverted.
    expect(parser.aborted).toEqual([segKey(0)]);
    expect(entryOf(store, segKey(0))!.state).toBe("listed");

    // Late parse completion: dropped.
    parser.pending[0]!.resolve();
    await settle();
    expect(entryOf(store, segKey(0))!.state).toBe("listed");
    expect(entryOf(store, segKey(0))!.trace).toBeUndefined();
    expect(win.stats().residentRawBytes).toBe(0);
  });
});

// ── HARD EDGE: re-entry hits the raw cache, not the network ─────────────

describe("hard edge: two-level cache", () => {
  it("re-entering an evicted window re-parses from cached bytes with ZERO new fetches", async () => {
    const segs = makeSegments(3, 25); // estimate = 25 x 4 = 100
    const store = makeStore();
    const fetcher = makeFetcher();
    const parser = makeParser({ tag: fetcher.tag, rawByteLength: 300 });
    const idle = makeIdle();
    const win = createSegmentWindow(store, segs, {
      fetchBytes: fetcher.fetchBytes,
      parser: parser.parser,
      idle: idle.idle,
      residentBudgetBytes: 500, // trigger 450
    });

    win.setViewport(viewOver(segs, 0));
    await settle();
    expect(entryOf(store, segKey(0))!.state).toBe("parsed");

    // Move to segment 2: reservations force segment 0 out (unprotected).
    win.setViewport(viewOver(segs, 2));
    await settle();
    expect(entryOf(store, segKey(0))!.state).toBe("evicted");
    // Eviction keeps the invariants and the learned raw size, drops the
    // parse itself.
    expect(entryOf(store, segKey(0))!.trace).toBeUndefined();
    expect(entryOf(store, segKey(0))!.edgePolls).toBeUndefined();
    expect(entryOf(store, segKey(0))!.rawByteLength).toBe(300);
    expect(entryOf(store, segKey(0))!.invariants).toBeDefined();

    // Re-enter: the raw bytes are still cached - parse, don't download.
    const fetchesBefore = fetcher.calls.filter((u) => u === segKey(0)).length;
    expect(fetchesBefore).toBe(1);
    win.setViewport(viewOver(segs, 0));
    await settle();
    expect(entryOf(store, segKey(0))!.state).toBe("parsed");
    expect(fetcher.calls.filter((u) => u === segKey(0)).length).toBe(1); // unchanged
    expect(parser.started.filter((u) => u === segKey(0)).length).toBe(2);
    expect(win.stats().cacheHits).toBe(1);
  });

  it("once the gzip cache evicts the bytes, re-entry re-downloads (its own LRU budget)", async () => {
    const segs = makeSegments(3, 25);
    const store = makeStore();
    const fetcher = makeFetcher({ byteLength: 16 });
    const parser = makeParser({ tag: fetcher.tag, rawByteLength: 300 });
    const idle = makeIdle();
    const win = createSegmentWindow(store, segs, {
      fetchBytes: fetcher.fetchBytes,
      parser: parser.parser,
      idle: idle.idle,
      residentBudgetBytes: 500,
      gzipCacheBudgetBytes: 30, // trigger 27: two 16-byte entries do not fit
    });

    win.setViewport(viewOver(segs, 0));
    await settle();
    win.setViewport(viewOver(segs, 2));
    await settle();
    expect(win.stats().gzipCacheBytes).toBe(16); // seg 0's bytes were LRU-evicted

    win.setViewport(viewOver(segs, 0));
    await settle();
    expect(fetcher.calls.filter((u) => u === segKey(0)).length).toBe(2);
    expect(entryOf(store, segKey(0))!.state).toBe("parsed");
  });
});

// ── HARD EDGE: eviction keeps resident raw under budget ─────────────────

describe("hard edge: budget eviction", () => {
  it("evicts farthest-first once the 90% trigger (minus reservations) is crossed; resident never exceeds the budget", async () => {
    const segs = makeSegments(6, 25); // estimates 100 each
    const store = makeStore();
    const fetcher = makeFetcher();
    const parser = makeParser({ tag: fetcher.tag, rawByteLength: 300 });
    const idle = makeIdle();
    const budget = 1000; // trigger 900
    const win = createSegmentWindow(store, segs, {
      fetchBytes: fetcher.fetchBytes,
      parser: parser.parser,
      idle: idle.idle,
      residentBudgetBytes: budget,
    });

    const residents: number[] = [];
    for (const i of [0, 1, 2, 3]) {
      win.setViewport(viewOver(segs, i));
      await settle();
      residents.push(win.stats().residentRawBytes);
      expect(win.stats().residentRawBytes).toBeLessThanOrEqual(budget);
    }
    // 300 -> 600 -> 900 (at the trigger, nothing evicted) -> then the
    // next step's reservation-aware pass frees room before parsing.
    expect(residents[0]).toBe(300);
    expect(residents[1]).toBe(600);
    expect(win.stats().evictions).toBeGreaterThan(0);
    expect(win.stats().peakResidentRawBytes).toBeLessThanOrEqual(budget);

    // The evicted entries kept their invariants (lanes/axes stability).
    const evicted = segs.filter((s) => entryOf(store, s.key)!.state === "evicted");
    expect(evicted.length).toBeGreaterThan(0);
    for (const s of evicted) {
      const entry = entryOf(store, s.key)!;
      expect(entry.invariants).toBeDefined();
      expect(entry.rawByteLength).toBe(300);
      expect(entry.trace).toBeUndefined();
    }
  });
});

// ── HARD EDGE: boundary polls truncated, not long ────────────────────────

describe("hard edge: boundary-poll truncation through the orchestrator", () => {
  const eventsFor = (url: string | undefined): TraceEvent[] => {
    switch (url) {
      case segKey(0):
        // Ends mid-poll (task 5): its PollEnd lives in segment 1.
        return [pollStart(10, 1, 5)];
      case segKey(1):
        // Starts with the dangling close, ends mid-poll (task 6).
        return [pollEnd(70, 1), pollStart(80, 1, 6)];
      case segKey(2):
        // Starts with task 6's close.
        return [pollEnd(130, 1), pollStart(140, 1), pollEnd(150, 1)];
      default:
        return [];
    }
  };

  it("a poll crossing the window edge is truncated at observed data; parsing the neighbor stitches it", async () => {
    const segs = makeSegments(3);
    const store = makeStore();
    const fetcher = makeFetcher();
    const parser = makeParser({ tag: fetcher.tag, events: eventsFor });
    const idle = makeIdle();
    const win = createSegmentWindow(store, segs, {
      fetchBytes: fetcher.fetchBytes,
      parser: parser.parser,
      idle: idle.idle,
    });

    // Window = segments 0+1; segment 2 stays unfetched.
    win.setViewport(range(segs[0]!.extent.startNs + 1e9, segs[1]!.extent.startNs + 1e9));
    await settle();

    const polls = win.boundaryPolls();
    // Task 5's poll spans the INTERNAL resident boundary: stitched whole.
    expect(polls.stitched).toEqual([
      {
        workerId: 1,
        start: 10,
        end: 70,
        taskId: 5,
        spawnLocId: "loc-5",
        spawnLoc: "src/task5.rs:1",
      },
    ]);
    // Task 6's poll crosses into UNFETCHED segment 2: explicitly
    // truncated at the last observed event (end === 80, its own start) -
    // never rendered as a poll running to the window edge or beyond.
    expect(polls.truncated).toEqual([
      {
        workerId: 1,
        start: 80,
        end: 80,
        taskId: 6,
        spawnLocId: "loc-6",
        spawnLoc: "src/task6.rs:1",
        truncatedAt: "end",
        openEnded: true,
      },
    ]);

    // Widen the window over segment 2: the truncation resolves into a
    // stitched poll; nothing remains truncated (segment 2 is the listing
    // end - core trace-edge parity).
    win.setViewport(range(segs[0]!.extent.startNs + 1e9, segs[2]!.extent.startNs + 1e9));
    await settle();
    const after = win.boundaryPolls();
    expect(after.truncated).toEqual([]);
    expect(after.stitched.map((p) => p.taskId).sort()).toEqual([5, 6]);
    expect(after.stitched.find((p) => p.taskId === 6)).toMatchObject({
      start: 80,
      end: 130,
    });
  });
});

// ── Prefetch at idle priority ────────────────────────────────────────────

describe("prefetch", () => {
  it("fetches +/-1 neighbors only when the idle scheduler fires", async () => {
    const segs = makeSegments(5);
    const store = makeStore();
    const fetcher = makeFetcher();
    const parser = makeParser({ tag: fetcher.tag });
    const idle = makeIdle();
    const win = createSegmentWindow(store, segs, {
      fetchBytes: fetcher.fetchBytes,
      parser: parser.parser,
      idle: idle.idle,
    });

    win.setViewport(viewOver(segs, 2));
    await settle();
    expect(fetcher.calls).toEqual([segKey(2)]);
    expect(idle.queuedCount()).toBe(2);

    idle.runIdles();
    await settle();
    expect(fetcher.calls.sort()).toEqual([segKey(1), segKey(2), segKey(3)]);
    expect(entryOf(store, segKey(1))!.state).toBe("parsed");
    expect(entryOf(store, segKey(3))!.state).toBe("parsed");
  });

  it("a stale idle callback (viewport moved on) starts nothing", async () => {
    const segs = makeSegments(8);
    const store = makeStore();
    const fetcher = makeFetcher();
    const parser = makeParser({ tag: fetcher.tag });
    const idle = makeIdle();
    const win = createSegmentWindow(store, segs, {
      fetchBytes: fetcher.fetchBytes,
      parser: parser.parser,
      idle: idle.idle,
    });

    win.setViewport(viewOver(segs, 2)); // queues prefetch for 1 and 3
    await settle();
    win.setViewport(viewOver(segs, 6)); // 1 and 3 are stale now
    await settle();
    idle.runIdles();
    await settle();
    expect(fetcher.calls).not.toContain(segKey(1));
    expect(fetcher.calls).not.toContain(segKey(3));
    // The fresh prefetch neighbors did start.
    expect(fetcher.calls).toContain(segKey(5));
    expect(fetcher.calls).toContain(segKey(7));
  });
});

// ── Failure handling ─────────────────────────────────────────────────────

describe("failure handling", () => {
  it("reverts the entry, reports once, and retries only after leaving the wanted set", async () => {
    const segs = makeSegments(6);
    const store = makeStore();
    const fetcher = makeFetcher();
    let failuresLeft = 1;
    const failingFetch: SegmentBytesFetcher = (url, o) => {
      if (url === segKey(0) && failuresLeft > 0) {
        failuresLeft -= 1;
        fetcher.calls.push(url);
        return Promise.reject(new Error("HTTP 500 fetching " + url));
      }
      return fetcher.fetchBytes(url, o);
    };
    const parser = makeParser({ tag: fetcher.tag });
    const idle = makeIdle();
    const onError = vi.fn();
    const win = createSegmentWindow(store, segs, {
      fetchBytes: failingFetch,
      parser: parser.parser,
      idle: idle.idle,
      onError,
    });

    win.setViewport(viewOver(segs, 0));
    await settle();
    expect(entryOf(store, segKey(0))!.state).toBe("listed");
    expect(onError).toHaveBeenCalledTimes(1);
    expect(onError).toHaveBeenCalledWith(segKey(0), expect.any(Error));

    // Same viewport again: NOT hot-retried.
    win.setViewport(viewOver(segs, 0));
    await settle();
    expect(fetcher.calls.filter((u) => u === segKey(0)).length).toBe(1);

    // Leave and re-enter: the failure mark clears and the fetch retries.
    win.setViewport(viewOver(segs, 4));
    await settle();
    win.setViewport(viewOver(segs, 0));
    await settle();
    expect(fetcher.calls.filter((u) => u === segKey(0)).length).toBe(2);
    expect(entryOf(store, segKey(0))!.state).toBe("parsed");
  });
});

// ── DoD: the 10-segment end-to-end scenario ──────────────────────────────

describe("10-segment scenario (DoD)", () => {
  it("navigates end-to-end and back with resident raw <= 128 MB throughout and zero re-downloads", async () => {
    // Recorded-size accounting: the parser REPORTS 30 MB decompressed per
    // segment (10 MB gzip in the listing) without allocating it - the
    // budget accountant only ever adds/compares recorded byteLengths.
    // The real-parse anchor test below ties the same accounting to actual
    // buffers at demo scale.
    const segs = makeSegments(10, 10 * MB);
    const store = makeStore();
    const fetcher = makeFetcher({ byteLength: 16 });
    const parser = makeParser({ tag: fetcher.tag, rawByteLength: 30 * MB });
    const idle = makeIdle();
    const win = createSegmentWindow(store, segs, {
      fetchBytes: fetcher.fetchBytes,
      parser: parser.parser,
      idle: idle.idle,
    });

    const step = async (i: number): Promise<void> => {
      win.setViewport(viewOver(segs, i));
      await settle();
      idle.runIdles(); // boundary prefetch at idle priority
      await settle();
      expect(win.stats().residentRawBytes).toBeLessThanOrEqual(
        RESIDENT_RAW_BUDGET_BYTES
      );
    };

    // Forward pass: 0 -> 9.
    for (let i = 0; i < 10; i++) await step(i);

    const forward = win.stats();
    expect(forward.peakResidentRawBytes).toBeLessThanOrEqual(RESIDENT_RAW_BUDGET_BYTES);
    // Every segment was fetched exactly once on the way out.
    expect(forward.networkFetches).toBe(10);
    expect(forward.evictions).toBeGreaterThan(0);
    // Every segment was parsed at least once and keeps its invariants.
    for (const s of segs) {
      expect(entryOf(store, s.key)!.invariants).toBeDefined();
      expect(entryOf(store, s.key)!.rawByteLength).toBe(30 * MB);
    }

    // Back pass: 9 -> 0. All gzip bytes are cached (10 tiny fixtures);
    // re-entry must be parse-only.
    for (let i = 9; i >= 0; i--) await step(i);

    const back = win.stats();
    expect(back.networkFetches).toBe(10); // ZERO re-downloads
    expect(back.cacheHits).toBe(back.parsesStarted - 10);
    expect(back.parsesStarted).toBeGreaterThan(forward.parsesStarted);
    expect(back.peakResidentRawBytes).toBeLessThanOrEqual(RESIDENT_RAW_BUDGET_BYTES);
    win.dispose();
  });
});

// ── Real-parse anchor: accounting tied to actual buffers ────────────────

describe("real-parse anchor (demo trace through the real core)", () => {
  it("accounts exactly the real decompressed sizes for two demo-trace segments", async () => {
    const fileBytes = readFileSync(
      fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url))
    );
    const rawTrace =
      fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
        ? new Uint8Array(gunzipSync(fileBytes))
        : new Uint8Array(fileBytes);
    const gzTrace = new Uint8Array(gzipSync(rawTrace));
    const direct = await parseTraceBuffer(rawTrace);

    const segs = makeSegments(2, gzTrace.byteLength);
    const store = makeStore();
    // Real gzipped bytes; a real (main-thread) parser implementing the
    // SegmentParser seam over the frozen core.
    const fetchBytes: SegmentBytesFetcher = () =>
      Promise.resolve(
        gzTrace.buffer.slice(
          gzTrace.byteOffset,
          gzTrace.byteOffset + gzTrace.byteLength
        ) as ArrayBuffer
      );
    const realParser: SegmentParser = (bytes) => {
      const done = (async (): Promise<SegmentParseResult> => {
        const raw = gunzipSync(bytes);
        const trace = await parseTraceBuffer(new Uint8Array(raw));
        return { trace, rawByteLength: raw.byteLength };
      })();
      return { done, abort: () => {} };
    };
    const idle = makeIdle();
    const win = createSegmentWindow(store, segs, {
      fetchBytes,
      parser: realParser,
      idle: idle.idle,
    });

    win.setViewport(range(segs[0]!.extent.startNs, segs[1]!.extent.endNs));
    await vi.waitFor(
      () => {
        expect(entryOf(store, segKey(0))!.state).toBe("parsed");
        expect(entryOf(store, segKey(1))!.state).toBe("parsed");
      },
      { timeout: 60_000 }
    );

    for (const i of [0, 1]) {
      const entry = entryOf(store, segKey(i))!;
      // The recorded size IS the real decompressed byte length.
      expect(entry.rawByteLength).toBe(rawTrace.byteLength);
      expect(entry.trace!.events.length).toBe(direct.events.length);
      expect(entry.invariants!.minTs).toBe(direct.minTs);
      expect(entry.invariants!.maxTs).toBe(direct.maxTs);
      expect(entry.invariants!.workerIds.length).toBeGreaterThan(0);
      expect(entry.edgePolls).toBeDefined();
    }
    expect(win.stats().residentRawBytes).toBe(2 * rawTrace.byteLength);
    expect(win.stats().gzipCacheBytes).toBe(2 * gzTrace.byteLength);
  }, 120_000);
});

// ── parseSegmentInWorker: the worker driver vs a scripted port ───────────

describe("parseSegmentInWorker", () => {
  function makeFakePort() {
    const sent: TraceWorkerRequest[] = [];
    let onMsg: ((message: TraceWorkerResponse) => void) | null = null;
    let onErr: ((error: unknown) => void) | null = null;
    let terminated = 0;
    return {
      port: {
        postMessage: (message: TraceWorkerRequest): void => {
          sent.push(message);
        },
        onMessage: (fn: (message: TraceWorkerResponse) => void): void => {
          onMsg = fn;
        },
        onError: (fn: (error: unknown) => void): void => {
          onErr = fn;
        },
        terminate: (): void => {
          terminated += 1;
        },
      },
      sent,
      emit: (message: TraceWorkerResponse): void => onMsg?.(message),
      emitError: (error: unknown): void => onErr?.(error),
      terminatedCount: () => terminated,
    };
  }

  const timing: TraceWorkerTiming = {
    startMs: 1,
    fetchDoneMs: null,
    parseDoneMs: 2,
    mode: "stream",
    events: 1,
    bytes: 42,
  };
  const fakeTrace = { events: [] } as unknown as ParsedTrace;

  it("posts one parse-buffer request with the exact bytes (sub-views sliced out)", () => {
    const fake = makeFakePort();
    const backing = new Uint8Array([0, 1, 2, 3, 4, 5]);
    const view = backing.subarray(2, 5);
    parseSegmentInWorker(view, { worker: () => fake.port, maxEvents: 7 });
    expect(fake.sent.length).toBe(1);
    const request = fake.sent[0]!;
    expect(request.kind).toBe("parse-buffer");
    if (request.kind !== "parse-buffer") throw new Error("unreachable");
    expect([...new Uint8Array(request.buffer)]).toEqual([2, 3, 4]);
    expect(request.parse).toEqual({ maxEvents: 7 });
  });

  it("done: resolves with the trace and the decompressed byteLength, terminates", async () => {
    const fake = makeFakePort();
    const job = parseSegmentInWorker(new Uint8Array(4), { worker: () => fake.port });
    fake.emit({
      kind: "done",
      trace: fakeTrace,
      buffer: new ArrayBuffer(42),
      mode: "stream",
      timing,
    });
    const result = await job.done;
    expect(result.trace).toBe(fakeTrace);
    expect(result.rawByteLength).toBe(42);
    expect(fake.terminatedCount()).toBe(1);
  });

  it("abort(): abort message + terminate + AbortError; late messages dropped", async () => {
    const fake = makeFakePort();
    const onProgress = vi.fn();
    const job = parseSegmentInWorker(new Uint8Array(4), {
      worker: () => fake.port,
      onProgress,
    });
    job.abort();
    expect(fake.sent.some((m) => m.kind === "abort")).toBe(true);
    expect(fake.terminatedCount()).toBe(1);
    await expect(job.done).rejects.toMatchObject({ name: "AbortError" });
    fake.emit({
      kind: "progress",
      progress: {
        phase: "parsing",
        mode: "stream",
        urlCount: 1,
        bytesRead: 1,
        totalBytes: null,
        eventCount: 0,
        startMs: 0,
        elapsedMs: 1,
      },
    });
    fake.emit({
      kind: "done",
      trace: fakeTrace,
      buffer: new ArrayBuffer(1),
      mode: "stream",
      timing,
    });
    expect(onProgress).not.toHaveBeenCalled();
    job.abort(); // idempotent
    expect(fake.terminatedCount()).toBe(1);
  });

  it("worker error: rejects with the name preserved; transport error rejects too", async () => {
    const fake = makeFakePort();
    const job = parseSegmentInWorker(new Uint8Array(4), { worker: () => fake.port });
    fake.emit({ kind: "error", message: "boom", name: "RangeError" });
    await expect(job.done).rejects.toMatchObject({ message: "boom", name: "RangeError" });
    expect(fake.terminatedCount()).toBe(1);

    const fake2 = makeFakePort();
    const job2 = parseSegmentInWorker(new Uint8Array(4), { worker: () => fake2.port });
    fake2.emitError(new Error("thread crashed"));
    await expect(job2.done).rejects.toMatchObject({ message: "thread crashed" });
  });
});
