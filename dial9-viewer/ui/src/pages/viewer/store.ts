// The viewer page's store instance: one createStore over the full StoreState.
// The shell subscribes to slice sets and renders declaratively from state; it
// never mutates a slice in place (the store dev-freezes them).
//
// Initial slice values are the "no trace loaded" resting state. Every foldable
// panel starts expanded (panelCollapsed all false) - the unified column shows
// analysis surfaces by default.

import { createStore } from "../../store/store.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import type { StoreOptions } from "../../store/store.js";

/** The default persistent inspector width (CSS px). */
export const DEFAULT_INSPECTOR_WIDTH = 360;

/** The default issues/tasks rail width (CSS px), drag-resizable at its right
 *  edge. Matches the pre-resize fixed width so existing layouts are unchanged
 *  until the user drags. */
export const DEFAULT_RAIL_WIDTH = 300;

/** Default height of the worker-lanes box (CSS px) - shows ~6 fixed rows before
 *  the box scrolls; the user resizes it via the lanes bottom gutter. Roomy by
 *  default so multi-runtime traces are not cramped to a couple of visible rows;
 *  folding a runtime (header click) reclaims the rest. */
export const DEFAULT_LANES_HEIGHT = 360;

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
      taskDump: null,
      sidebarRange: null,
      hoveredWakerTaskId: null,
      spawnedTasksRange: null,
    },
    // POI / issues-rail controls. Defaults: filter = "sched" and worst-first ON
    // (sort by the duration column, desc). No POI is current until the user
    // steps (`n`/`p`) or clicks a rail row.
    poi: {
      filter: "sched",
      sortKey: "duration",
      sortDir: "desc",
      index: -1,
      railTab: "issues",
      taskSort: "total",
      taskSortDir: "desc",
      taskIndex: -1,
    },
    uiPrefs: {
      // Analysis surfaces are visible by default. The unified column has no
      // per-panel folds; every surface starts expanded.
      panelCollapsed: { spans: false, events: false, cpu: false, queue: false },
      // Track management. Empty resting state: the catalogue order
      // (track-layout.ts) and nothing collapsed. hydrateTrackPrefs (main.ts)
      // overlays the persisted order/collapse on boot; the store stays pure
      // (no localStorage here, so it is Node-testable).
      trackOrder: [],
      collapsed: {},
      collapsedRuntimes: {},
      collapsedRuntimeMetrics: {},
      sidebarWidth: DEFAULT_INSPECTOR_WIDTH,
      railWidth: DEFAULT_RAIL_WIDTH,
      taskColWidths: {},
      issueColWidths: {},
      lanesViewportHeight: DEFAULT_LANES_HEIGHT,
      lanesScrollTop: 0,
      selectedSpanNames: new Set<string>(),
      selectedEventNames: new Set<string>(),
      // Span filter resting state: no text, no percentile floor.
      spanFilter: "",
      spanPctFilter: 0,
      // Resting clock mode: relative offsets in UTC. The toolbar toggles mutate
      // these; the time-axis track reads them.
      timeMode: "rel",
      tz: "utc",
      // Stack lists start as lists; the toggle in the poll/blocking views flips
      // this and it persists across selections.
      stacksAsFlamegraph: false,
    },
    view: {
      fieldCharts: [],
      inspectorTab: "task",
      expandedPollGroups: new Set<string>(),
      pollFlamegraphSection: "cpu",
      pollWorkerZoom: [],
      pollOffworkerZoom: [],
      relatedCollapsed: {},
      relatedExpand: {},
      relatedCorrelate: null,
      regionMode: null,
      regionHeapMode: "bytes",
      regionGroupBy: "leaf",
      regionWorkerZoom: [],
      regionOffworkerZoom: [],
      regionInspectFocus: null,
      spanNavIndex: -1,
    },
    transient: {
      mouseNs: null,
      hoverEventTs: null,
      drag: null,
      keyboardSelection: null,
      // At-cursor readout: null until the pointer hovers the draw area over a
      // loaded trace.
      atCursor: null,
    },
    segments: { segments: new Map() },
  };
}

/** Create the viewer store (optionally with an injected frame scheduler). */
export function createViewerStore(options?: StoreOptions): ViewerStore {
  return createStore<StoreState>(initialViewerState(), options);
}
