// T24 at-cursor readout DoD: the readout compute mirrors the legacy info-panel
// (I6), the T17 coverage signal maps the covering segment's residency, and the
// contract is POPULATED on the transient channel during hover (a subscriber -
// the stub / T31 - sees transient.atCursor). Node-pure + a store instance.

import { describe, it, expect } from "vitest";
import { computeAtCursorReadout, coverageAt, type AtCursorInput } from "./readout.js";
import { createViewerStore } from "../../pages/viewer/store.js";
import type { AtCursorReadout, SegmentEntry } from "../../types/state.js";

// ── computeAtCursorReadout (I6) ──────────────────────────────────────────

describe("computeAtCursorReadout (I6 info-panel parity)", () => {
  const input: AtCursorInput = {
    workerIds: [0, 1],
    queueSamples: [
      { t: 0, global: 1 },
      { t: 100, global: 5 },
      { t: 200, global: 3 },
    ],
    workerQueueSamples: {
      0: [
        { t: 0, local: 1 },
        { t: 100, local: 4 },
      ],
      1: [
        { t: 0, local: 2 },
        { t: 100, local: 9 },
      ],
    },
    activeTaskSamples: [
      { t: 0, count: 1 },
      { t: 50, count: 3 },
      { t: 150, count: 2 },
    ],
  };

  it("nearest global queue, MAX local across workers, step active-task count", () => {
    const r = computeAtCursorReadout(input, 90, 0, "complete");
    expect(r).toEqual<AtCursorReadout>({
      ns: 90,
      workerId: 0,
      globalQueue: 5, // nearest sample to 90 is t=100
      localMax: 9, // max(worker0=4, worker1=9) at nearest t=100
      activeTaskCount: 3, // latest sample with t <= 90 is t=50
      coverage: "complete",
    });
  });

  it("null series -> null readings (info-panel omits the missing lines)", () => {
    const empty: AtCursorInput = {
      workerIds: [0],
      queueSamples: [],
      workerQueueSamples: { 0: [] },
      activeTaskSamples: [],
    };
    const r = computeAtCursorReadout(empty, 42, null, "complete");
    expect(r.globalQueue).toBeNull();
    expect(r.localMax).toBeNull();
    expect(r.activeTaskCount).toBeNull();
    expect(r.workerId).toBeNull();
  });
});

// ── coverageAt (T17 carried obligation) ──────────────────────────────────

describe("coverageAt (T17 windowed-data completeness)", () => {
  const entry = (state: SegmentEntry["state"], startNs: number, endNs: number): SegmentEntry => ({
    state,
    extent: { startNs, endNs },
    sizeBytes: 1_000,
  });

  it("empty map (whole-trace resident path) -> complete", () => {
    expect(coverageAt(new Map(), 500)).toBe("complete");
  });

  it("covering segment residency maps onto the coverage signal", () => {
    expect(coverageAt(new Map([["a", entry("parsed", 0, 1000)]]), 500)).toBe("complete");
    expect(coverageAt(new Map([["a", entry("oversized", 0, 1000)]]), 500)).toBe("oversized");
    expect(coverageAt(new Map([["a", entry("evicted", 0, 1000)]]), 500)).toBe("truncated");
    expect(coverageAt(new Map([["a", entry("listed", 0, 1000)]]), 500)).toBe("truncated");
    expect(coverageAt(new Map([["a", entry("fetching", 0, 1000)]]), 500)).toBe("truncated");
  });

  it("a cursor over no segment (gap) is not reported as truncated", () => {
    expect(coverageAt(new Map([["a", entry("evicted", 0, 100)]]), 500)).toBe("complete");
  });
});

// ── store contract populated during hover ────────────────────────────────

describe("transient.atCursor store contract", () => {
  it("a transient update delivers the readout to a transient subscriber", () => {
    // A collecting scheduler so we can flush the single coalesced tick.
    const frames: (() => void)[] = [];
    const store = createViewerStore({ scheduler: (cb) => frames.push(cb) });

    let seen: AtCursorReadout | null | undefined;
    store.subscribe(["transient"], (state) => {
      seen = state.transient.atCursor;
    });

    const readout: AtCursorReadout = {
      ns: 4_200,
      workerId: 1,
      globalQueue: 3,
      localMax: 2,
      activeTaskCount: 5,
      coverage: "complete",
    };
    store.update("transient", { mouseNs: 4_200, atCursor: readout });

    // getState reflects it synchronously (the store contract).
    expect(store.getState().transient.atCursor).toEqual(readout);

    // The subscriber receives it on the coalesced frame tick.
    expect(frames.length).toBe(1);
    frames[0]!();
    expect(seen).toEqual(readout);
  });

  it("clearing on mouseleave nulls the readout", () => {
    const frames: (() => void)[] = [];
    const store = createViewerStore({ scheduler: (cb) => frames.push(cb) });
    store.update("transient", {
      mouseNs: 1,
      atCursor: { ns: 1, workerId: 0, globalQueue: 0, localMax: 0, activeTaskCount: null, coverage: "complete" },
    });
    store.update("transient", { mouseNs: null, atCursor: null });
    expect(store.getState().transient.atCursor).toBeNull();
    expect(store.getState().transient.mouseNs).toBeNull();
  });
});
