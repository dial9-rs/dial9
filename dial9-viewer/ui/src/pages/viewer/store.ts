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
      sidebarRange: null,
      hoveredWakerTaskId: null,
    },
    uiPrefs: {
      // S1 amendment: analysis surfaces are visible by default. The unified
      // column has no per-panel folds; every surface starts expanded.
      panelCollapsed: { spans: false, events: false, cpu: false, queue: false },
      sidebarWidth: DEFAULT_INSPECTOR_WIDTH,
      selectedSpanNames: new Set<string>(),
      selectedEventNames: new Set<string>(),
    },
    transient: {
      mouseNs: null,
      hoverEventTs: null,
      drag: null,
      keyboardSelection: null,
    },
    segments: { segments: new Map() },
  };
}

/** Create the viewer store (optionally with an injected frame scheduler). */
export function createViewerStore(options?: StoreOptions): ViewerStore {
  return createStore<StoreState>(initialViewerState(), options);
}
