// Behavioral store-integration tests for the minimap's navigation dispatch
// (T35 DoD "drag/click behavioral tests"). The pure pointer math is covered in
// minimap-model.test.ts; here we assert the component's navigation commit moves
// the store viewport - including into an UNFETCHED tier-1-only tail (the
// headline scenario). The canvas render + DOM pointer wiring are browser-driven
// (listed in HANDOFF).

import { describe, it, expect } from "vitest";
import { createViewerStore, initialViewerState } from "./store.js";
import { minimapNavigate } from "./minimap.js";
import {
  deriveMinimapRange,
  fracToNs,
  grabOffsetFor,
} from "./minimap-model.js";
import type { ViewerStore } from "../../store/store.js";
import type { SegmentEntry, SegmentLifecycle, StoreState } from "../../types/state.js";

/** A manual scheduler so store updates flush synchronously in the test. */
function manualStore(): { store: ViewerStore; flush(): void } {
  const pending: (() => void)[] = [];
  const store = createViewerStore({ scheduler: (cb) => pending.push(cb) });
  return {
    store,
    flush() {
      for (const cb of pending.splice(0)) cb();
    },
  };
}

/** A 10-segment set (1s each) with fetching suppressed beyond `fetchedThrough`. */
function multiSegmentState(fetchedThrough: number): StoreState {
  const s = initialViewerState();
  const segNs = 1_000_000_000;
  const segments = new Map<string, SegmentEntry>();
  for (let i = 0; i < 10; i++) {
    const state: SegmentLifecycle = i <= fetchedThrough ? "parsed" : "listed";
    segments.set(`seg-${i}.bin`, {
      state,
      extent: { startNs: i * segNs, endNs: (i + 1) * segNs },
      sizeBytes: 1_000,
    });
  }
  s.segments = { segments };
  // Navigable bounds span the whole listing; the view starts over the head.
  s.viewport = { viewStart: 0, viewEnd: segNs, minTs: 0, maxTs: 10 * segNs };
  return s;
}

describe("minimapNavigate - click", () => {
  it("centers the view on the clicked timestamp (preserving width)", () => {
    const { store, flush } = manualStore();
    const state = multiSegmentState(4);
    store.update("viewport", state.viewport);
    store.update("segments", state.segments);
    flush();
    const range = deriveMinimapRange(state.viewport, state.segments.segments)!;
    minimapNavigate(store, range, 1_000_000_000, { kind: "click", ns: 5_500_000_000 });
    flush();
    const vp = store.getState().viewport;
    expect(vp.viewStart).toBe(5_000_000_000);
    expect(vp.viewEnd).toBe(6_000_000_000);
  });
});

describe("minimapNavigate - navigating into an UNFETCHED tail (headline)", () => {
  it("moves the viewport into the tier-1-only tail via a click at the tail", () => {
    const { store, flush } = manualStore();
    const state = multiSegmentState(4); // segments 5..9 are listed (unfetched)
    store.update("viewport", state.viewport);
    store.update("segments", state.segments);
    flush();
    const range = deriveMinimapRange(state.viewport, state.segments.segments)!;
    expect(range).toEqual({ startNs: 0, endNs: 10_000_000_000 });

    // Click at fraction 0.85 -> ~8.5s, deep in the unfetched tail.
    const targetNs = fracToNs(range, 0.85);
    minimapNavigate(store, range, state.viewport.viewEnd - state.viewport.viewStart, {
      kind: "click",
      ns: targetNs,
    });
    flush();

    const vp = store.getState().viewport;
    // The viewport landed in the tail (segments 5..9), which stays navigable
    // even though its data is not fetched (tier-1 extents only).
    expect(vp.viewStart).toBeGreaterThan(7_000_000_000);
    expect(vp.viewEnd).toBeLessThanOrEqual(range.endNs);
    // The covering segment there is still "listed" (unfetched) - navigation
    // does not require residency.
    const covering = [...state.segments.segments.values()].find(
      (seg) => vp.viewStart >= seg.extent.startNs && vp.viewStart < seg.extent.endNs,
    );
    expect(covering?.state).toBe("listed");
  });
});

describe("minimapNavigate - drag", () => {
  it("scrubs the box keeping the grabbed point under the pointer", () => {
    const { store, flush } = manualStore();
    const state = multiSegmentState(9);
    store.update("viewport", state.viewport);
    store.update("segments", state.segments);
    flush();
    const range = deriveMinimapRange(state.viewport, state.segments.segments)!;
    const width = state.viewport.viewEnd - state.viewport.viewStart;

    // Grab inside the box (view [0,1s]) at 0.2s, drag pointer to 6.2s.
    const grab = grabOffsetFor(
      { viewStart: state.viewport.viewStart, viewEnd: state.viewport.viewEnd },
      width,
      200_000_000,
    );
    expect(grab).toBe(200_000_000);
    minimapNavigate(store, range, width, {
      kind: "drag",
      grabOffsetNs: grab,
      ns: 6_200_000_000,
    });
    flush();
    const vp = store.getState().viewport;
    expect(vp.viewStart).toBe(6_000_000_000);
    expect(vp.viewEnd).toBe(7_000_000_000);
  });

  it("clamps a drag at the range's right edge", () => {
    const { store, flush } = manualStore();
    const state = multiSegmentState(9);
    store.update("viewport", state.viewport);
    store.update("segments", state.segments);
    flush();
    const range = deriveMinimapRange(state.viewport, state.segments.segments)!;
    const width = state.viewport.viewEnd - state.viewport.viewStart;
    minimapNavigate(store, range, width, {
      kind: "drag",
      grabOffsetNs: 0,
      ns: 20_000_000_000, // way past the end
    });
    flush();
    const vp = store.getState().viewport;
    expect(vp.viewEnd).toBe(range.endNs);
    expect(vp.viewEnd - vp.viewStart).toBe(width);
  });
});
