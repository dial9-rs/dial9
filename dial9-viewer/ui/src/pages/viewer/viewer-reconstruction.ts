// Viewer deep-link reconstruction: boot-time URL hydration plus the explicit
// transition that applies a parsed trace and resolves trace-dependent anchors.
//
// The store remains the authority. The URL is an input at boot and a projection
// afterward; callers do not need to order trace subscribers so a full-fit runs
// before a URL viewport restore.

import type { ParsedTrace } from "../../lib/trace/index.js";
import { resolveViewState } from "../../lib/url/index.js";
import type { ViewerStore } from "../../store/store.js";
import type { SelectionSlice } from "../../types/state.js";
import { focusWindow, readFocusLink } from "./focus-link.js";
import { resolveFocusLink, resolveUrlSelection } from "./url-selection.js";
import {
  hydrateViewerStore,
  readViewerUrlState,
  type ViewerUrlState,
} from "./url-state.js";
import { taskIndexFor } from "./tasks-model.js";
import { traceDisplayBounds, type TraceDisplayBounds } from "./trace-bounds.js";
import {
  derivePoiViewModel,
  poiIndexForAnchor,
  poiIndexForPoll,
} from "./poi.js";

export type LoadedTraceKind = "source" | "reparse";

export interface ViewerReconstruction {
  /** Decoded query state needed by boot-time view adapters and the loader. */
  readonly urlState: ViewerUrlState;
  /**
   * Commit one successfully parsed trace and reconstruct its shareable view.
   * `source` means a newly loaded source; `reparse` means Set/Clear Range over
   * the same source.
   */
  applyLoadedTrace(trace: ParsedTrace, kind: LoadedTraceKind): void;
}

export interface ViewerLocation {
  search: string;
  hash: string;
}

/**
 * Hydrate trace-independent URL state now and return the explicit transition
 * used when parsing produces a trace.
 */
export function createViewerReconstruction(
  store: ViewerStore,
  location: ViewerLocation,
): ViewerReconstruction {
  const urlState = readViewerUrlState(location.search);
  const sharedState = resolveViewState(location);
  const focusLink = readFocusLink(location.search);
  hydrateViewerStore(store, urlState, sharedState);

  let firstTrace = true;

  function fitTrace(trace: ParsedTrace): TraceDisplayBounds | null {
    const bounds = traceDisplayBounds(trace);
    if (bounds !== null) {
      store.update("viewport", {
        minTs: bounds.minTs,
        maxTs: bounds.maxTs,
        viewStart: bounds.minTs,
        viewEnd: bounds.maxTs,
      });
      return bounds;
    }
    store.update("viewport", {
      minTs: 0,
      maxTs: 0,
      viewStart: 0,
      viewEnd: 0,
    });
    return null;
  }

  function restoreInitialTrace(trace: ParsedTrace): void {
    const bounds = fitTrace(trace);
    let restoredCanonicalViewport = false;
    if (
      bounds !== null &&
      urlState.viewStart !== undefined &&
      urlState.viewEnd !== undefined
    ) {
      const viewStart = Math.max(bounds.minTs, urlState.viewStart);
      const viewEnd = Math.min(bounds.maxTs, urlState.viewEnd);
      if (viewEnd > viewStart) {
        store.update("viewport", { viewStart, viewEnd });
        restoredCanonicalViewport = true;
      }
    }
    const selection = resolveUrlSelection(trace, urlState);
    if (
      urlState.poiAnchor !== undefined ||
      (
        focusLink !== null &&
        urlState.poiFilter !== undefined &&
        urlState.poll !== undefined
      )
    ) {
      const state = store.getState();
      const sorted = derivePoiViewModel(
        trace,
        state.poi,
        state.viewport.minTs,
      ).sorted;
      const index = urlState.poiAnchor
        ? poiIndexForAnchor(sorted, urlState.poiAnchor)
        : selection.pollDetail != null
          ? poiIndexForPoll(sorted, selection.pollDetail)
          : -1;
      if (index >= 0) {
        store.update("poi", { index });
      }
    }
    const hasCanonicalSelection =
      Object.keys(selection).length > 0 ||
      taskExists(trace, urlState.selectedTaskId);
    const activeFocusLink =
      !restoredCanonicalViewport && !hasCanonicalSelection
        ? focusLink
        : null;

    // focus_* is a one-shot bootstrap for aggregate exemplars. Once a URL has
    // canonical viewport or selection anchors, those describe the current
    // navigated state and must win over the original exemplar.
    if (activeFocusLink !== null) {
      const focused = resolveFocusLink(trace, activeFocusLink);
      if (focused !== null) {
        Object.assign(selection, focused.patch);
        store.update("viewport", focused.viewport);
      } else {
        const window = focusWindow(activeFocusLink, trace.clockOffsetNs);
        if (Number.isFinite(window.start)) {
          const pad = Math.max((window.end - window.start) * 2, 1e6);
          store.update("viewport", {
            viewStart: Math.max(bounds?.minTs ?? window.start, window.start - pad),
            viewEnd: Math.min(bounds?.maxTs ?? window.end, window.end + pad),
          });
        }
      }
    }

    // An explicit shareable task selection wins, followed by the exemplar's
    // focus_task. Keep a task inferred from a matched span only when neither
    // URL task anchor resolves against this trace.
    for (const taskId of [urlState.selectedTaskId, activeFocusLink?.taskId]) {
      if (taskExists(trace, taskId)) {
        selection.selectedTaskId = taskId;
        break;
      }
    }
    if (Object.keys(selection).length > 0) {
      store.update("selection", selection);
    }
  }

  function selectionAnchors(selection: Readonly<SelectionSlice>): ViewerUrlState {
    return {
      ...(selection.selectedTaskId !== null
        ? { selectedTaskId: selection.selectedTaskId }
        : {}),
      ...(selection.spanFocus !== null
        ? { selectedSpanId: selection.spanFocus.spanId }
        : {}),
      ...(selection.focusedSpanId !== null
        ? { focusedSpanId: selection.focusedSpanId }
        : {}),
      ...(selection.pollDetail !== null
        ? {
            poll: {
              startNs: selection.pollDetail.start,
              taskId: selection.pollDetail.taskId,
            },
          }
        : {}),
      ...(selection.taskDump !== null
        ? {
            taskDump: {
              taskId: selection.taskDump.taskId,
              timestamps: [...selection.taskDump.timestamps],
            },
          }
        : {}),
      ...(selection.pinnedEvent !== null
        ? { pinnedEventTs: selection.pinnedEvent.timestamp }
        : {}),
      ...(selection.sidebarRange !== null
        ? { sidebarRange: selection.sidebarRange }
        : {}),
      ...(selection.spawnedTasksRange !== null
        ? { spawnedRange: selection.spawnedTasksRange }
        : {}),
    };
  }

  function taskExists(trace: ParsedTrace, taskId: number | undefined): taskId is number {
    return (
      taskId !== undefined &&
      taskIndexFor(trace).rows.some((row) => row.taskId === taskId)
    );
  }

  function replaceSelection(
    trace: ParsedTrace,
    anchors: ViewerUrlState,
  ): void {
    const resolved = resolveUrlSelection(trace, anchors);
    store.update("selection", {
      selectedTaskId: taskExists(trace, anchors.selectedTaskId)
        ? anchors.selectedTaskId
        : null,
      spanFocus: null,
      focusedSpanId: null,
      pinnedEvent: null,
      pollDetail: null,
      taskDump: null,
      sidebarRange: null,
      hoveredWakerTaskId: null,
      spawnedTasksRange: null,
      ...resolved,
    });
  }

  function clearSelection(): void {
    store.update("selection", {
      selectedTaskId: null,
      spanFocus: null,
      focusedSpanId: null,
      pinnedEvent: null,
      pollDetail: null,
      taskDump: null,
      sidebarRange: null,
      hoveredWakerTaskId: null,
      spawnedTasksRange: null,
    });
  }

  function resetSourceScopedState(): void {
    clearSelection();
    store.update("poi", {
      index: -1,
      taskIndex: -1,
    });
    store.update("view", {
      inspectorTab: "task",
      expandedPollGroups: new Set<string>(),
      pollFlamegraphSection: "cpu",
      pollWorkerZoom: [],
      pollOffworkerZoom: [],
      relatedCollapsed: {},
      relatedExpand: {},
      relatedCorrelate: null,
      regionWorkerZoom: [],
      regionOffworkerZoom: [],
      regionInspectFocus: null,
      spanNavIndex: -1,
    });
  }

  return {
    urlState,
    applyLoadedTrace(trace, kind): void {
      const reparseAnchors =
        !firstTrace && kind === "reparse"
          ? selectionAnchors(store.getState().selection)
          : null;
      store.update("trace", { trace });
      if (!firstTrace) {
        fitTrace(trace);
        if (reparseAnchors !== null) {
          replaceSelection(trace, reparseAnchors);
        } else {
          resetSourceScopedState();
        }
        return;
      }
      firstTrace = false;
      restoreInitialTrace(trace);
    },
  };
}
