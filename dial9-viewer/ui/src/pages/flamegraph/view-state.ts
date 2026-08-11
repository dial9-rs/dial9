// The flamegraph page's URL view-state wiring, in two halves:
//
// - restoreFgStateFromUrl: on load, resolve the effective view state from the
//   URL (versioned hash wins per field, stable query params fill the gaps) and
//   drive the widget's applyViewState (zoom + inspect focus + search + spawn/
//   runtime filters). Restore is SILENT: the widget suspends its onViewChange
//   while applying, so restoring produces zero URL writes and keeps a shared
//   link byte-stable on open.
//
// - createFgUrlSync: user view change -> store slice update -> ONE debounced
//   history.replaceState carrying BOTH the stable query params and the
//   versioned hash.
//
// Kept DOM-free (URL host, timer and frame scheduler injectable) so the
// restore-on-load and view->URL paths are integration-testable under plain Node.

import { createStore, type FrameScheduler } from "../../store/store.js";
import type { FlamegraphViewState } from "../../lib/canvas/index.js";
import {
  applyLegacyZoomToQuery,
  bindViewStateToUrl,
  resolveViewState,
  type DebounceTimer,
  type UrlHost,
  type ViewState,
} from "../../lib/url/index.js";

/** Structural subset of FlamegraphInstance the restore path drives. */
export interface FgStateTarget {
  applyViewState(state: FlamegraphViewState, opts?: { silent?: boolean }): void;
}

/** Build the widget's view-state shape from a resolved URL ViewState. */
function viewStateToWidget(state: ViewState): FlamegraphViewState {
  const out: FlamegraphViewState = {};
  if (state.fgWorkerZoom !== undefined) out.workerZoom = state.fgWorkerZoom;
  if (state.fgOffworkerZoom !== undefined) out.offworkerZoom = state.fgOffworkerZoom;
  if (state.fgInspect !== undefined) {
    out.inspect = {
      name: state.fgInspect,
      fullName: state.fgInspectFull ?? state.fgInspect,
    };
  }
  if (state.fgSearch !== undefined) out.search = state.fgSearch;
  if (state.fgSpawn !== undefined) out.spawn = state.fgSpawn;
  if (state.fgRuntime !== undefined) out.runtime = state.fgRuntime;
  return out;
}

/**
 * Restore the flamegraph view from the URL (only when the time-range filter
 * reproduced the shared tree - a fallback trace has a different one). Drives the
 * widget's applyViewState silently, so restoring writes nothing back to the URL.
 * Returns the resolved state for callers that want to inspect it.
 */
export function restoreFgStateFromUrl(
  url: { search: string; hash: string },
  fg: FgStateTarget,
  timeRangeMatched: boolean,
): ViewState {
  if (!timeRangeMatched) return {};
  const state = resolveViewState(url);
  const widgetState = viewStateToWidget(state);
  fg.applyViewState(widgetState, { silent: true });
  return state;
}

/** The page-local store shape: one slice carrying the full view state. */
export interface FgViewSlice {
  workerZoom: readonly string[];
  offworkerZoom: readonly string[];
  inspect: { name: string; fullName: string } | null;
  search: string;
  spawn: string;
  runtime: string;
}
export interface FgPageState {
  fgView: FgViewSlice;
}

/** The empty view slice (nothing zoomed/inspected/filtered). */
function emptyFgViewSlice(): FgViewSlice {
  return {
    workerZoom: [],
    offworkerZoom: [],
    inspect: null,
    search: "",
    spawn: "",
    runtime: "",
  };
}

/** Snapshot the widget's live view state into a store slice. */
function widgetToSlice(vs: FlamegraphViewState): FgViewSlice {
  return {
    workerZoom: vs.workerZoom ?? [],
    offworkerZoom: vs.offworkerZoom ?? [],
    inspect: vs.inspect ?? null,
    search: vs.search ?? "",
    spawn: vs.spawn ?? "",
    runtime: vs.runtime ?? "",
  };
}

function fgViewToViewState(v: FgViewSlice): ViewState {
  const out: ViewState = {};
  if (v.workerZoom.length > 0) out.fgWorkerZoom = v.workerZoom;
  if (v.offworkerZoom.length > 0) out.fgOffworkerZoom = v.offworkerZoom;
  if (v.inspect) {
    out.fgInspect = v.inspect.name;
    out.fgInspectFull = v.inspect.fullName || v.inspect.name;
  }
  if (v.search) out.fgSearch = v.search;
  if (v.spawn) out.fgSpawn = v.spawn;
  if (v.runtime) out.fgRuntime = v.runtime;
  return out;
}

export interface FgUrlSyncOptions {
  host?: UrlHost;
  timer?: DebounceTimer;
  debounceMs?: number;
  /** Store frame scheduler; defaults to requestAnimationFrame. */
  scheduler?: FrameScheduler;
}

export interface FgUrlSync {
  /** Pass as createFlamegraph's onViewChange callback. */
  onViewChange(): void;
  /** Write pending state now (copy-link calls this before copying). */
  flush(): void;
  dispose(): void;
}

/**
 * Wire user view changes to the URL through a page-local store slice.
 * `getViewState` is read lazily on each change (the widget's live state is the
 * source of truth), so the sync can be constructed before the widget -
 * createFlamegraph needs the callback at creation time.
 */
export function createFgUrlSync(
  getViewState: () => FlamegraphViewState,
  options: FgUrlSyncOptions = {},
): FgUrlSync {
  const store = createStore<FgPageState>(
    { fgView: emptyFgViewSlice() },
    options.scheduler !== undefined ? { scheduler: options.scheduler } : {},
  );
  const binding = bindViewStateToUrl(store, {
    slices: ["fgView"],
    project: (s) => fgViewToViewState(s.fgView),
    // Keep the stable query params live alongside the hash.
    mirrorToQuery: applyLegacyZoomToQuery,
    ...(options.host !== undefined ? { host: options.host } : {}),
    ...(options.timer !== undefined ? { timer: options.timer } : {}),
    ...(options.debounceMs !== undefined ? { debounceMs: options.debounceMs } : {}),
  });
  return {
    onViewChange() {
      store.update("fgView", widgetToSlice(getViewState()));
    },
    flush() {
      binding.flush();
    },
    dispose() {
      binding.dispose();
    },
  };
}
