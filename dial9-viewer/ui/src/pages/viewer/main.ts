// The trace-viewer page entry. Wires the app shell (shell.ts) to the viewer
// store, mounts the app-wide chrome (help overlay, toasts, ARIA announcer,
// esc-cascade mechanism), and kicks off the initial trace load so the shell
// renders on real data. Track/panel content mounts its own store subscribers
// against the store created here.

import "../../styles/viewer.css";
import { mountKeyRouter } from "../../lib/interact/index.js";
import { getAnnouncer } from "../../lib/interact/announce.js";
import { createViewerStore } from "./store.js";
import {
  hydrateTrackPrefs,
  mountTrackPrefsPersistence,
} from "./track-management.js";
import { createEscCascade, ESC_PRIORITY } from "./esc-cascade.js";
import { mountViewerHelp } from "./help.js";
import { createToasts } from "./toasts.js";
import { mountShell } from "./shell.js";
import { mountMinimap } from "./minimap.js";
import { createStatusBar } from "./status-bar.js";
import { mountLoadChrome } from "./load-chrome.js";
import { mountInspector } from "./inspector.js";
import { createRegionAnalysis } from "./region-analysis.js";
import { mountLanes } from "../../components/canvas/lanes/index.js";
import { mountOverlay } from "../../components/overlay/index.js";
import { mountLaneInteraction } from "./lane-interaction.js";
import { initViewportFromTrace } from "./viewport-init.js";
import { readKeyDerivedIdentity } from "../../lib/trace/index.js";
import {
  bindViewStateToUrl,
  resolveViewState,
  type ViewStateBinding,
} from "../../lib/url/index.js";
import {
  projectViewerState,
  mirrorViewerToQuery,
  readViewerUrlState,
} from "./url-state.js";
import { resolveUrlSelection } from "./url-selection.js";
import { mountViewerSearch } from "./search-overlay.js";
import { buildSearchIndex, searchWindow } from "./search-model.js";
import type { SearchResult } from "./search-model.js";
import { poiJump } from "./poi.js";
import type { StoreState } from "../../types/state.js";

// Dual-UI switch: render the always-visible "Switch to legacy UI" pill. The
// <head> auto-boot is a no-op on this off-root new-UI path.
if (window.D9UiSwitch) {
  window.D9UiSwitch.mount({ side: "new" });
} else {
  // One-time load-order/serving problem, not a loop - log it loudly.
  console.warn("ui-switch.js is not loaded; the UI switch control is unavailable");
}

boot();

function boot(): void {
  const root = document.getElementById("app");
  if (root === null) {
    throw new Error("viewer shell: #app mount point missing");
  }

  const store = createViewerStore();

  // Track management persistence (survives reload). Hydrate the saved track
  // order + collapse map into uiPrefs BEFORE the shell mounts so the first
  // paint reflects them, then subscribe to persist future changes.
  hydrateTrackPrefs(store);
  const disposeTrackPrefs = mountTrackPrefsPersistence(store);

  // Restore shareable state from the URL. The hash carries tm/tz; readable
  // query params carry the rest. Boot-time prefs (tm/tz, span filter, track
  // order/collapse) apply now - OVER the localStorage hydrate above, so a
  // shared URL wins. The viewport window + task selection apply after the trace
  // loads (below), once minTs/maxTs and the task set exist.
  const urlView = readViewerUrlState(window.location.search);
  const urlHash = resolveViewState({
    search: window.location.search,
    hash: window.location.hash,
  });
  const bootPrefs: Partial<StoreState["uiPrefs"]> = {};
  if (urlHash.timeMode !== undefined) bootPrefs.timeMode = urlHash.timeMode;
  if (urlHash.timeZone !== undefined) bootPrefs.tz = urlHash.timeZone;
  if (urlView.spanFilter !== undefined) bootPrefs.spanFilter = urlView.spanFilter;
  if (urlView.trackOrder !== undefined) bootPrefs.trackOrder = urlView.trackOrder;
  if (urlView.collapsed !== undefined) {
    bootPrefs.collapsed = Object.fromEntries(
      urlView.collapsed.map((id) => [id, true]),
    );
  }
  if (urlView.spanPct !== undefined) bootPrefs.spanPctFilter = urlView.spanPct;
  if (urlView.spanNames !== undefined) {
    bootPrefs.selectedSpanNames = new Set(urlView.spanNames);
  }
  if (urlView.eventNames !== undefined) {
    bootPrefs.selectedEventNames = new Set(urlView.eventNames);
  }
  if (Object.keys(bootPrefs).length > 0) store.update("uiPrefs", bootPrefs);

  // Issues-rail restore. Filter/sort/index are plain values (index maps into
  // the trace-derived list once it exists, so setting it now is safe); no part
  // needs the trace to be valid.
  const poiPatch: Partial<StoreState["poi"]> = {};
  if (urlView.poiFilter !== undefined) poiPatch.filter = urlView.poiFilter;
  if (urlView.poiSort !== undefined) {
    poiPatch.sortKey = urlView.poiSort.key;
    poiPatch.sortDir = urlView.poiSort.dir;
  }
  if (urlView.poiIndex !== undefined) poiPatch.index = urlView.poiIndex;
  if (Object.keys(poiPatch).length > 0) store.update("poi", poiPatch);

  // The URL sync binding, assigned after mount; forward-referenced by the
  // status bar's copy-link flush so a copy always reflects the live state.
  let urlBinding: ViewStateBinding | null = null;

  // ARIA live region: mount it now so screen-reader announcements (keyboard
  // selection start/complete, zoom confirmations) have a target.
  const announcer = getAnnouncer();

  // App chrome: esc-cascade mechanism + help overlay + toasts.
  const esc = createEscCascade();
  const help = mountViewerHelp(document, esc);

  // `/` search palette: a per-trace index (via store.derived), the modal, and
  // a jump on pick.
  const searchIndex = store.derived(["trace"], (s) =>
    s.trace.trace ? buildSearchIndex(s.trace.trace) : null,
  );
  const search = mountViewerSearch(document, esc, {
    getIndex: () => searchIndex(),
    onPick: (r) => navigateToSearchResult(r),
  });
  function navigateToSearchResult(r: SearchResult): void {
    const vp = store.getState().viewport;
    if (r.nav.kind === "poi") {
      const j = poiJump(r.nav.poi, vp);
      store.update("viewport", { viewStart: j.viewStart, viewEnd: j.viewEnd });
      if (j.selectedTaskId !== null) {
        store.update("selection", { selectedTaskId: j.selectedTaskId });
      }
      return;
    }
    const win = searchWindow(r.nav.startNs, r.nav.endNs, vp.minTs, vp.maxTs);
    store.update("viewport", { viewStart: win.start, viewEnd: win.end });
    if (r.nav.kind === "task") {
      store.update("selection", { selectedTaskId: r.nav.taskId });
    } else {
      store.update("selection", {
        spanFocus: { spanId: r.nav.spanId, chain: new Set([r.nav.spanId]) },
        focusedSpanId: r.nav.spanId,
      });
    }
  }

  const source = traceSource(window.location.search);
  // Forward references resolved after the shell mounts: the "New File" button
  // opens the load chrome (created below - it needs the shell's toast region
  // first), and the toolbar's analysis / range-reparse surfaces dispatch
  // through toasts.
  let loadChrome: ReturnType<typeof mountLoadChrome> | null = null;
  let toastsRef: ReturnType<typeof createToasts> | null = null;
  // Forward ref: the toolbar analysis buttons open the region analyses, but the
  // region panel needs the shell's inspector region + toast region, created
  // below. The buttons only fire on user click (post-mount).
  let regionPanel: ReturnType<typeof createRegionAnalysis> | null = null;
  const notify = (message: string): void => {
    toastsRef?.show({ id: "t33-seam", type: "info", message });
  };
  const shell = mountShell(root, store, {
    toggleHelp: () => help.toggle(),
    sourceLabel: () => loadChrome?.currentLabel() ?? source.label,
    // Key-derived svc/host from the browser handoff. A boot constant: the
    // toolbar reconciles it against the trace-embedded metadata.
    keyDerivedIdentity: readKeyDerivedIdentity(window.location.search),
    onNewFile: () => loadChrome?.requestNewFile(),
    onOpenAnalysis: (kind) => regionPanel?.openWholeTrace(kind),
    // Set Range: re-parse the loaded trace filtered to the current view, off
    // the main thread; the reparsed trace's extent becomes the new bounds
    // (initViewportFromTrace refits). Clear Range: re-parse the full trace.
    onSetRange: (range) => loadChrome?.reparseToRange(range),
    onClearRange: () => loadChrome?.reparseToRange(null),
  });
  const toasts = createToasts(shell.toastRegion);
  toastsRef = toasts;

  // Region-analysis panel: flamegraph / blocking-calls / heap for a Shift+drag
  // region or a whole-trace toolbar open. Renders into the inspector Stack tab
  // (passed as an inspector dep below). Created after the toast region so
  // pop-out/no-trace errors can surface.
  regionPanel = createRegionAnalysis(store, {
    inspectorHost: shell.inspectorRegion,
    notify,
    announcer,
  });

  // Worker-lanes track content: mounts AFTER the shell so its store
  // subscription runs after the shell's chrome render each frame, and claims
  // the lanes canvas so the shell stops painting its placeholder.
  const lanes = mountLanes(shell.trackColumn, store);

  // Transient overlay: crosshair canvas + hover tooltip + at-cursor readout, on
  // the store's `transient` RAF channel. Mounts AFTER the shell + lanes so its
  // subscriber runs LAST each frame - it re-ensures the overlay canvas after
  // any shell re-render that would clobber it, and reads column geometry only
  // after the shell's writes have settled.
  const overlay = mountOverlay(root, shell.trackColumn, store);

  // Persistent inspector sidebar: tabs (Task/Poll/Event/Related/Stack), the
  // at-cursor readout, and the P-row mechanics (resize/persist). Rendered
  // imperatively into the shell's empty inspector aside; re-scopes to the
  // selection in the same action. Registered with the esc-cascade so its
  // content selection clears before the entry's task-selection fallback.
  const inspector = mountInspector(shell.inspectorRegion, store, { esc, regionPanel });

  // Overview minimap: tier-1 density + POI ticks + a draggable viewport box,
  // filling the shell's minimap host. Drag/click dispatch store viewport
  // updates (never a direct render). It renders from the segments slice + the
  // whole-trace event histogram; the aggregate seam stays open.
  const minimap = mountMinimap(shell.minimapRegion, store);

  // Status bar: selection line, view range, segment fetch/parse progress, the
  // copy-link button, and the key hints. The copy-link copies the live URL,
  // passing a `beforeCopyLink` flush so it reflects live state.
  const statusBar = createStatusBar(shell.statusRegion, store, {
    clearSelection: () => {
      store.update("selection", {
        selectedTaskId: null,
        spanFocus: null,
        focusedSpanId: null,
        pinnedEvent: null,
      });
    },
    // Flush the debounced URL write so copy-link copies the live state.
    beforeCopyLink: () => urlBinding?.flush(),
  });

  // Initialize the viewport from the trace the moment it loads. Registered
  // BEFORE the lane interaction so its zoom-history baseline records the
  // fitted view (both subscribe to `trace`; order = registration order).
  initViewportFromTrace(store);

  // Apply the URL's viewport window + task selection to the FIRST loaded trace.
  // Registered AFTER initViewportFromTrace so it overrides the full-fit;
  // one-shot, so a later Set-Range reparse refits to its own extent instead of
  // snapping back to the shared window.
  {
    let applied = false;
    const unsubUrlRestore = store.subscribe(["trace"], (state) => {
      const trace = state.trace.trace;
      if (applied || trace === null) return;
      applied = true;
      unsubUrlRestore();
      if (urlView.viewStart !== undefined && urlView.viewEnd !== undefined) {
        store.update("viewport", {
          viewStart: urlView.viewStart,
          viewEnd: urlView.viewEnd,
        });
      }
      // Re-resolve the canvas-selection anchors (span/poll/event/region/
      // spawned) against the loaded trace, plus the task, into one patch.
      // Unresolvable anchors are silently dropped.
      const selPatch = resolveUrlSelection(trace, urlView);
      if (urlView.selectedTaskId !== undefined) {
        selPatch.selectedTaskId = urlView.selectedTaskId;
      }
      if (Object.keys(selPatch).length > 0) {
        store.update("selection", selPatch);
      }
    });
  }

  // Lane interaction: pointer pan/zoom/region gestures, wheel zoom,
  // click-select, the selection overlay, and the viewport controls. Its key
  // bindings (arrows/WASD/z + the kb-selection Escape/Enter) join the unified
  // router BEFORE the entry's `?`/Escape fallbacks so a kb-selection
  // cancel/confirm wins, and the generic Escape cascade runs otherwise.
  const laneInteraction = mountLaneInteraction(root, shell.trackColumn, store, { announcer });

  // Unified keyboard: the lane bindings first, then `?` toggles help and Escape
  // runs the cascade + clears the task selection + refocuses the timeline.
  mountKeyRouter(window, [
    ...laneInteraction.keyBindings,
    // Toolbar + issues-rail accelerators: `n`/`p` step the POI rail, `g`
    // focuses the goto-time input. After the lane bindings so a live keyboard
    // selection still owns arrows/Enter/Escape.
    ...shell.keyBindings,
    // No inTextEntry: a `/` typed in a field types a slash; elsewhere it is
    // consumed, which also stops Firefox's native quick-find hijacking it.
    { key: "/", onKey: () => search.open() },
    { key: "?", onKey: () => help.toggle() },
    {
      key: "Escape",
      onKey: () => {
        if (esc.handle()) return true;
        if (store.getState().selection.selectedTaskId !== null) {
          store.update("selection", { selectedTaskId: null });
        }
        shell.trackColumn.focus();
        return true;
      },
    },
  ]);

  // Load chrome: the drop zone / file picker / drag-drop / URL loading surfaces
  // + the New File flow. It runs loads in the trace worker, registers its
  // Escape surface in the cascade, and auto-loads the boot `?trace=` components
  // when present; otherwise it shows the drop zone. Load failures surface as an
  // error toast.
  loadChrome = mountLoadChrome({
    store,
    esc,
    onError: (message) => {
      toasts.show({ id: "load-error", type: "error", message });
    },
    ...(source.urls.length > 0
      ? { initialUrls: source.urls, initialLabel: source.label }
      : {}),
  });

  // Mirror the shareable state INTO the URL as it changes, debounced.
  // Registered last so the boot-time restore above does not fight it; the
  // copy-link button flushes it first (beforeCopyLink) so a copy is current.
  urlBinding = bindViewStateToUrl(store, {
    slices: ["viewport", "selection", "uiPrefs", "poi"],
    project: projectViewerState,
    mirrorToQuery: mirrorViewerToQuery,
  });

  // Teardown hook for HMR / tests (not strictly needed in production).
  window.addEventListener("beforeunload", () => {
    urlBinding?.dispose();
    search.dispose();
    disposeTrackPrefs();
    loadChrome?.dispose();
    statusBar.dispose();
    minimap.dispose();
    laneInteraction.dispose();
    overlay.dispose();
    inspector.dispose();
    regionPanel?.dispose();
    lanes.dispose();
    shell.dispose();
    help.dispose();
  });
}

interface TraceSource {
  urls: readonly string[];
  label: string;
}

/**
 * Resolve the boot trace source from `?trace=` components. With none, the urls
 * are empty: the load chrome shows the drop zone and waits for a drop / pick /
 * demo instead of auto-loading.
 */
function traceSource(search: string): TraceSource {
  const params = new URLSearchParams(search);
  const raw = params.getAll("trace").filter((u) => u.length > 0);
  if (raw.length === 0) return { urls: [], label: "" };
  // Resolve each ?trace= value against the ORIGIN ROOT before it reaches the
  // worker. The worker's script URL is under /assets/ (or /src/lib/trace/worker/
  // in dev), so a bare relative value would resolve against the WORKER url and
  // fetch an HTML fallback, which the parser rejects with "Invalid trace
  // header". Root-relative ("/api/object?...") and absolute values pass through.
  const urls = raw.map((u) =>
    new URL(u, window.location.origin + "/").toString(),
  );
  const first = raw[0] ?? "trace";
  return { urls, label: lastPathSegment(first) };
}

function lastPathSegment(url: string): string {
  const noQuery = url.split("?")[0] ?? url;
  const parts = noQuery.split("/").filter((p) => p.length > 0);
  return parts[parts.length - 1] ?? url;
}
