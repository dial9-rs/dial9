// src/pages/flamegraph/view-state.ts - the flamegraph page's URL
// view-state wiring (T19; the codec's first consumer). Two halves:
//
// - restoreZoomFromUrl: on load, resolve the effective zoom from the URL
//   (versioned hash wins per field, legacy worker-zoom/offworker-zoom
//   query params fill the gaps - codec precedence) and drive the frozen
//   widget's zoomToPath. Restore stays OUTSIDE the store: the frozen
//   widget only fires onZoomChange for USER zooms (zoomToPath does not),
//   so restoring produces zero URL writes - exactly like the legacy page,
//   and what keeps a shared link byte-stable on open.
//
// - createFgUrlSync: user zoom -> store slice update -> ONE debounced
//   history.replaceState carrying BOTH the legacy query params (exact
//   F147-F153 semantics, so an address-bar copy still opens on the legacy
//   page) and the versioned hash (the unified carrier chunk-2 extends).
//
// This page has no clock-mode control, so it writes no tm/tz; the codec's
// vocabulary reserves them for the chunk-2 viewer (see the schema doc).
//
// Kept DOM-free (the fg widget is a structural interface; URL host, timer
// and frame scheduler injectable) so the restore-on-load and zoom->URL
// paths are integration-testable under plain Node - the in-browser twin
// is parity journey J9.

import { createStore, type FrameScheduler } from "../../store/store.js";
import {
  applyLegacyZoomToQuery,
  bindViewStateToUrl,
  resolveViewState,
  type DebounceTimer,
  type UrlHost,
  type ViewState,
} from "../../lib/url/index.js";

/** The two zoom paths, as the frozen widget reports them (getZoomPath). */
export interface FgZoomPaths {
  worker: string[];
  offworker: string[];
}

/** Structural subset of FlamegraphInstance the restore path needs. */
export interface FgZoomTarget {
  zoomToPath(key: "worker" | "offworker", names: readonly string[]): void;
}

/**
 * Restore zoom state from the URL (F151 gate: only when the time-range
 * filter reproduced the shared tree - a fallback trace has a different
 * one). Returns the resolved state for callers that want to inspect it.
 */
export function restoreZoomFromUrl(
  url: { search: string; hash: string },
  fg: FgZoomTarget,
  timeRangeMatched: boolean,
): ViewState {
  if (!timeRangeMatched) return {};
  const state = resolveViewState(url);
  if (state.fgWorkerZoom !== undefined) fg.zoomToPath("worker", state.fgWorkerZoom);
  if (state.fgOffworkerZoom !== undefined) fg.zoomToPath("offworker", state.fgOffworkerZoom);
  return state;
}

/** The page-local store shape: one slice carrying the zoom view state. */
export interface FgViewSlice {
  workerZoom: readonly string[];
  offworkerZoom: readonly string[];
}
export interface FgPageState {
  fgView: FgViewSlice;
}

function fgViewToViewState(v: FgViewSlice): ViewState {
  const out: ViewState = {};
  if (v.workerZoom.length > 0) out.fgWorkerZoom = v.workerZoom;
  if (v.offworkerZoom.length > 0) out.fgOffworkerZoom = v.offworkerZoom;
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
  /** Pass as createFlamegraph's onZoomChange callback. */
  onZoomChange(): void;
  /** Write pending state now (copy-link calls this before copying). */
  flush(): void;
  dispose(): void;
}

/**
 * Wire user zooms to the URL through a page-local store slice.
 * `getZoomPath` is read lazily on each zoom change (the widget's live
 * stacks are the source of truth), so the sync can be constructed before
 * the widget - createFlamegraph needs the callback at creation time.
 */
export function createFgUrlSync(
  getZoomPath: () => FgZoomPaths,
  options: FgUrlSyncOptions = {},
): FgUrlSync {
  const store = createStore<FgPageState>(
    { fgView: { workerZoom: [], offworkerZoom: [] } },
    options.scheduler !== undefined ? { scheduler: options.scheduler } : {},
  );
  const binding = bindViewStateToUrl(store, {
    slices: ["fgView"],
    project: (s) => fgViewToViewState(s.fgView),
    // Keep the legacy params live alongside the hash (F147-F153; N10).
    mirrorToQuery: applyLegacyZoomToQuery,
    ...(options.host !== undefined ? { host: options.host } : {}),
    ...(options.timer !== undefined ? { timer: options.timer } : {}),
    ...(options.debounceMs !== undefined ? { debounceMs: options.debounceMs } : {}),
  });
  return {
    onZoomChange() {
      const z = getZoomPath();
      store.update("fgView", { workerZoom: z.worker, offworkerZoom: z.offworker });
    },
    flush() {
      binding.flush();
    },
    dispose() {
      binding.dispose();
    },
  };
}
