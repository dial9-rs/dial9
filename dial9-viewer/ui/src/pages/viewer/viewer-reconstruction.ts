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

  function fitTrace(trace: ParsedTrace): void {
    if (trace.minTs !== null && trace.maxTs !== null) {
      store.update("viewport", {
        minTs: trace.minTs,
        maxTs: trace.maxTs,
        viewStart: trace.minTs,
        viewEnd: trace.maxTs,
      });
      return;
    }
    store.update("viewport", {
      minTs: 0,
      maxTs: 0,
      viewStart: 0,
      viewEnd: 0,
    });
  }

  function restoreInitialTrace(trace: ParsedTrace): void {
    fitTrace(trace);
    if (
      trace.minTs !== null &&
      trace.maxTs !== null &&
      urlState.viewStart !== undefined &&
      urlState.viewEnd !== undefined
    ) {
      const viewStart = Math.max(trace.minTs, urlState.viewStart);
      const viewEnd = Math.min(trace.maxTs, urlState.viewEnd);
      if (viewEnd > viewStart) {
        store.update("viewport", { viewStart, viewEnd });
      }
    }
    const selection = resolveUrlSelection(trace, urlState);

    if (focusLink !== null) {
      const focused = resolveFocusLink(trace, focusLink);
      if (focused !== null) {
        Object.assign(selection, focused.patch);
        store.update("viewport", focused.viewport);
      } else {
        const window = focusWindow(focusLink, trace.clockOffsetNs);
        if (Number.isFinite(window.start)) {
          const pad = Math.max((window.end - window.start) * 2, 1e6);
          store.update("viewport", {
            viewStart: Math.max(trace.minTs ?? window.start, window.start - pad),
            viewEnd: Math.min(trace.maxTs ?? window.end, window.end + pad),
          });
        }
      }
    }

    if (
      urlState.selectedTaskId !== undefined &&
      taskIndexFor(trace).rows.some(
        (row) => row.taskId === urlState.selectedTaskId,
      )
    ) {
      selection.selectedTaskId = urlState.selectedTaskId;
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
