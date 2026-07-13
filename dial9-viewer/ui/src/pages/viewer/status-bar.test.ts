// Store-binding tests for the status bar (T35): the DoD check "status bar
// reflects selection / range / progress (Vitest on store bindings)". The pure
// view model reads store state; the imperative mount + copy-link button are the
// browser-driven halves (listed in HANDOFF).

import { describe, it, expect } from "vitest";
import { initialViewerState } from "./store.js";
import {
  segmentProgress,
  selectionState,
  statusViewModel,
} from "./status-bar.js";
import type { StoreState, SegmentEntry, SegmentLifecycle } from "../../types/state.js";
import type { ParsedTrace } from "../../lib/trace/index.js";

function seg(startNs: number, endNs: number, state: SegmentLifecycle): SegmentEntry {
  return { state, extent: { startNs, endNs }, sizeBytes: 1_000 };
}

/** A resting store state with a (fake) trace resident. */
function loadedState(): StoreState {
  const s = initialViewerState();
  s.trace = { trace: {} as ParsedTrace };
  s.viewport = { viewStart: 1_000_000_000, viewEnd: 2_000_000_000, minTs: 0, maxTs: 5_000_000_000 };
  return s;
}

describe("selectionState (04 F7)", () => {
  it("reads a selected task as a hex label with a clear affordance", () => {
    const sel = { ...initialViewerState().selection, selectedTaskId: 0x2a };
    expect(selectionState(sel)).toEqual({ hasSelection: true, label: "Task 0x2a selected" });
  });

  it("falls back to span focus, then pinned event, then no selection", () => {
    const base = initialViewerState().selection;
    expect(
      selectionState({ ...base, spanFocus: { spanId: "s1", chain: new Set(["s1"]) } }),
    ).toEqual({ hasSelection: true, label: "Span s1 focused" });
    expect(
      selectionState({
        ...base,
        pinnedEvent: {
          events: [],
          timestamp: 0,
          taskId: null,
          name: "flush",
          poll: null,
          detailEvent: null,
        },
      }),
    ).toEqual({ hasSelection: true, label: 'Event "flush" pinned' });
    expect(selectionState(base)).toEqual({ hasSelection: false, label: "No selection" });
  });
});

describe("segmentProgress (2.8 feedback surface)", () => {
  it("is inactive with no segment windowing (empty slice)", () => {
    expect(segmentProgress({ segments: new Map() })).toEqual({ label: null, active: false });
  });

  it("counts resident segments and flags in-flight fetches", () => {
    const segments = new Map<string, SegmentEntry>([
      ["a", seg(0, 1000, "parsed")],
      ["b", seg(1000, 2000, "parsed")],
      ["c", seg(2000, 3000, "fetching")],
      ["d", seg(3000, 4000, "listed")],
    ]);
    expect(segmentProgress({ segments })).toEqual({
      label: "Segments 2/4 loaded · fetching 1",
      active: true,
    });
  });

  it("surfaces oversized segments and is inactive when nothing is fetching", () => {
    const segments = new Map<string, SegmentEntry>([
      ["a", seg(0, 1000, "parsed")],
      ["b", seg(1000, 2000, "oversized")],
    ]);
    expect(segmentProgress({ segments })).toEqual({
      label: "Segments 1/2 loaded · 1 oversized",
      active: false,
    });
  });
});

describe("statusViewModel (selection + range + progress bindings)", () => {
  it("reports 'no trace loaded' at rest", () => {
    const vm = statusViewModel(initialViewerState());
    expect(vm.viewRangeLabel).toBe("no trace loaded");
    expect(vm.selection.hasSelection).toBe(false);
    expect(vm.progress.label).toBeNull();
  });

  it("renders the visible range as offsets + duration off the trace start", () => {
    const vm = statusViewModel(loadedState());
    expect(vm.viewRangeLabel).toMatch(/^view \+1\.00s - \+2\.00s \(/);
  });

  it("reflects a live selection and segment progress together", () => {
    const s = loadedState();
    s.selection = { ...s.selection, selectedTaskId: 7 };
    s.segments = {
      segments: new Map<string, SegmentEntry>([
        ["a", seg(0, 1000, "parsed")],
        ["b", seg(1000, 2000, "fetching")],
      ]),
    };
    const vm = statusViewModel(s);
    expect(vm.selection.label).toBe("Task 0x7 selected");
    expect(vm.progress).toEqual({ label: "Segments 1/2 loaded · fetching 1", active: true });
  });
});
