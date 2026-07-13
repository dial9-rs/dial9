// src/pages/viewer/store.ts - the viewer page's store instance (T21;
// architecture 2.2, store mechanism T07).
//
// One createStore over the full StoreState (T06). The shell subscribes to
// slice sets and renders declaratively from state (N17: no DOM mutation
// outside a state-driven render); it never mutates a slice in place (the
// store dev-freezes them - N18/finding 1). Downstream chunk-2 tickets add
// their subscribers (tracks T22-T30, inspector T31, minimap/status T35)
// against THIS store.
//
// Initial slice values are the "no trace loaded" resting state. Two S1
// amendment decisions are expressed HERE as defaults, not in a renderer:
// every foldable panel starts EXPANDED (panelCollapsed all false) - the
// unified column shows analysis surfaces by default, the legacy
// all-collapsed default dies with the folds.

import { createStore } from "../../store/store.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import type { StoreOptions } from "../../store/store.js";

/** The default persistent inspector width (CSS px); concept-1 sidebar. */
export const DEFAULT_INSPECTOR_WIDTH = 360;

/** Build the viewer store's resting (no-trace) initial state. */
export function initialViewerState(): StoreState {
  return {
    trace: { trace: null },
    viewport: { viewStart: 0, viewEnd: 0, minTs: 0, maxTs: 0 },
    selection: {
      selectedTaskId: null,
      spanFocus: null,
      focusedSpanId: null,
      pinnedEvent: null,
      pollDetail: null,
      sidebarRange: null,
      hoveredWakerTaskId: null,
      spawnedTasksRange: null,
    },
    // POI / issues-rail controls (T33; features/02 C, 04 S5). Defaults mirror
    // the legacy toolbar resting state: filter = "sched" (the `#poi-filter`
    // first option) and worst-first ON (sort by the duration column, desc).
    // No POI is current until the user steps (`n`/`p`) or clicks a rail row.
    poi: {
      filter: "sched",
      sortKey: "duration",
      sortDir: "desc",
      index: -1,
    },
    uiPrefs: {
      // S1 amendment: analysis surfaces are visible by default. The unified
      // column has no per-panel folds; every surface starts expanded.
      panelCollapsed: { spans: false, events: false, cpu: false, queue: false },
      // Track management (T36; amended section O). Empty resting state: the
      // catalogue order (track-layout.ts) and nothing collapsed - i.e. the S1
      // "all analysis surfaces visible" default. hydrateTrackPrefs (main.ts)
      // overlays the persisted order/collapse on boot; the store stays pure
      // (no localStorage here, so it is Node-testable).
      trackOrder: [],
      collapsed: {},
      sidebarWidth: DEFAULT_INSPECTOR_WIDTH,
      selectedSpanNames: new Set<string>(),
      selectedEventNames: new Set<string>(),
      // Span filter resting state (02 J7/J8): no text, no percentile floor
      // (spans track T26).
      spanFilter: "",
      spanPctFilter: 0,
      // Legacy resting clock mode (viewer.html `useAbsoluteTime`/`useLocalTz`
      // both false): relative offsets in UTC. The toolbar toggles (T33)
      // mutate these; the time-axis track (T25) reads them.
      timeMode: "rel",
      tz: "utc",
    },
    transient: {
      mouseNs: null,
      hoverEventTs: null,
      drag: null,
      keyboardSelection: null,
      // At-cursor readout (T24; 02 I6 / 04 S4): null until the pointer hovers
      // the draw area over a loaded trace.
      atCursor: null,
    },
    segments: { segments: new Map() },
  };
}

/** Create the viewer store (optionally with an injected frame scheduler). */
export function createViewerStore(options?: StoreOptions): ViewerStore {
  return createStore<StoreState>(initialViewerState(), options);
}
