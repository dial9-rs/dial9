// src/pages/viewer/main.ts - the migrated trace-viewer page entry (T21;
// Vite entry new/viewer.html, ADR-0004 section 8 stage 2). Wires the app
// shell (shell.ts) to the viewer store, mounts the app-wide chrome (help
// overlay T, toasts U, ARIA announcer A16, esc-cascade mechanism), and
// kicks off the initial trace load so the shell renders on real data.
//
// T21 is the SHELL: this entry proves the chrome and the track SLOTS render
// on the demo trace. Track/panel CONTENT (T22-T35) mounts its own store
// subscribers against the store created here; the file-load UX (T34) will
// replace the bootstrap load below with the real drop-zone pipeline.

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
import type { StoreState } from "../../types/state.js";

// Dual-UI switch (T38): render the always-visible "Switch to legacy UI"
// pill. The <head> auto-boot is a no-op on this off-root new-UI path.
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

  // Track management persistence (T36; headline DoD: survives reload). Hydrate
  // the saved track order + collapse map into uiPrefs BEFORE the shell mounts
  // so the first paint reflects them, then subscribe to persist future
  // changes. Together they close the reload round-trip (hydrate reads what the
  // previous session's subscriber wrote).
  hydrateTrackPrefs(store);
  const disposeTrackPrefs = mountTrackPrefsPersistence(store);

  // Restore shareable state from the URL (#1). The hash carries tm/tz; readable
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
  if (Object.keys(bootPrefs).length > 0) store.update("uiPrefs", bootPrefs);

  // The URL sync binding (#1), assigned after mount; forward-referenced by the
  // status bar's copy-link flush so a copy always reflects the live state.
  let urlBinding: ViewStateBinding | null = null;

  // ARIA live region (A16): mount it now so screen-reader announcements
  // (keyboard selection start/complete, zoom confirmations) have a target.
  // T23 dispatches through it; T21 lands the region.
  const announcer = getAnnouncer();

  // App chrome: esc-cascade mechanism + help overlay + toasts.
  const esc = createEscCascade();
  const help = mountViewerHelp(document, esc);

  const source = traceSource(window.location.search);
  // Forward references resolved after the shell mounts: the "New File" button
  // opens the load chrome (created below - it needs the shell's toast region
  // first), and the toolbar's not-yet-landed surfaces (T31/T32 analysis
  // panels, T34 range re-parse) dispatch through toasts instead of harmful
  // placeholder state.
  let loadChrome: ReturnType<typeof mountLoadChrome> | null = null;
  let toastsRef: ReturnType<typeof createToasts> | null = null;
  // Forward ref: the toolbar analysis buttons (D1/D2/D3) open the T32 region
  // analyses, but the region panel needs the shell's inspector region + toast
  // region, created below. The buttons only fire on user click (post-mount).
  let regionPanel: ReturnType<typeof createRegionAnalysis> | null = null;
  const notify = (message: string): void => {
    toastsRef?.show({ id: "t33-seam", type: "info", message });
  };
  const shell = mountShell(root, store, {
    toggleHelp: () => help.toggle(),
    sourceLabel: () => loadChrome?.currentLabel() ?? source.label,
    // Key-derived svc/host from the S3 browser handoff (C1a; T45). A boot
    // constant: the toolbar reconciles it against the trace-embedded metadata.
    keyDerivedIdentity: readKeyDerivedIdentity(window.location.search),
    onNewFile: () => loadChrome?.requestNewFile(),
    onOpenAnalysis: (kind) => regionPanel?.openWholeTrace(kind),
    // Set Range (E3): re-parse the loaded trace filtered to the current view,
    // off the main thread; the reparsed trace's extent becomes the new bounds
    // (initViewportFromTrace refits). Clear Range (E4): re-parse the full trace.
    onSetRange: (range) => loadChrome?.reparseToRange(range),
    onClearRange: () => loadChrome?.reparseToRange(null),
  });
  const toasts = createToasts(shell.toastRegion);
  toastsRef = toasts;

  // Region-analysis panel (T32): flamegraph / blocking-calls / heap for a
  // Shift+drag region or a whole-trace toolbar open. Renders into T31's
  // inspector Stack tab (passed as an inspector dep below); driven by the
  // frozen flamegraph.js widget through its public surface. Created after the
  // toast region so pop-out/no-trace errors can surface.
  regionPanel = createRegionAnalysis(store, {
    inspectorHost: shell.inspectorRegion,
    notify,
    announcer,
  });

  // Worker-lanes track content (T22): mounts AFTER the shell so its store
  // subscription runs after the shell's chrome render each frame, and claims
  // the lanes canvas so the shell stops painting its placeholder.
  const lanes = mountLanes(shell.trackColumn, store);

  // Transient overlay (T24): crosshair canvas + hover tooltip + at-cursor
  // readout, on the store's `transient` RAF channel. Mounts AFTER the shell +
  // lanes so its subscriber runs LAST each frame - it re-ensures the overlay
  // canvas after any shell re-render that would clobber it, and reads column
  // geometry only after the shell's writes have settled.
  const overlay = mountOverlay(root, shell.trackColumn, store);

  // Persistent inspector sidebar (T31): tabs (Task/Poll/Event/Related/Stack),
  // the at-cursor readout (04 S4), and the P-row mechanics (resize/persist).
  // Rendered imperatively into the shell's empty inspector aside; re-scopes to
  // the selection in the same action (S4). Registered with the esc-cascade so
  // its content selection clears before the entry's task-selection fallback.
  const inspector = mountInspector(shell.inspectorRegion, store, { esc, regionPanel });

  // Overview minimap (T35): tier-1 density + POI ticks + a draggable viewport
  // box, filling the shell's minimap host. Drag/click dispatch store viewport
  // updates (never a direct render). No live aggregate source is wired in the
  // viewer yet, so it renders from the T17 segments slice + the whole-trace
  // event histogram; the aggregate seam stays open for a scope-aware ticket.
  const minimap = mountMinimap(shell.minimapRegion, store);

  // Status bar (T35): selection line (04 F7), view range, segment fetch/parse
  // progress (the 2.8 feedback surface), the copy-link button (T19), and the
  // key hints. The copy-link copies the live URL; when the T19 view-state URL
  // sync is wired into the viewer it passes a `beforeCopyLink` flush here.
  const statusBar = createStatusBar(shell.statusRegion, store, {
    clearSelection: () => {
      store.update("selection", {
        selectedTaskId: null,
        spanFocus: null,
        focusedSpanId: null,
        pinnedEvent: null,
      });
    },
    // Flush the debounced URL write so copy-link copies the live state (#1).
    beforeCopyLink: () => urlBinding?.flush(),
  });

  // Initialize the viewport from the trace the moment it loads. Registered
  // BEFORE the lane interaction so its zoom-history baseline records the
  // fitted view (both subscribe to `trace`; order = registration order).
  initViewportFromTrace(store);

  // Apply the URL's viewport window + task selection to the FIRST loaded trace
  // (#1). Registered AFTER initViewportFromTrace so it overrides the full-fit;
  // one-shot, so a later Set-Range reparse refits to its own extent instead of
  // snapping back to the shared window.
  if (urlView.viewStart !== undefined || urlView.selectedTaskId !== undefined) {
    let applied = false;
    const unsubUrlRestore = store.subscribe(["trace"], (state) => {
      if (applied || state.trace.trace === null) return;
      applied = true;
      unsubUrlRestore();
      if (urlView.viewStart !== undefined && urlView.viewEnd !== undefined) {
        store.update("viewport", {
          viewStart: urlView.viewStart,
          viewEnd: urlView.viewEnd,
        });
      }
      if (urlView.selectedTaskId !== undefined) {
        store.update("selection", { selectedTaskId: urlView.selectedTaskId });
      }
    });
  }

  // Lane interaction (T23): pointer pan/zoom/region gestures, wheel zoom,
  // click-select, the H10 selection overlay, and the H1-H4 viewport controls.
  // Its key bindings (arrows/WASD/z + the kb-selection Escape/Enter) join the
  // unified router BEFORE the entry's `?`/Escape fallbacks so a kb-selection
  // cancel/confirm wins, and the generic Escape cascade runs otherwise.
  const laneInteraction = mountLaneInteraction(root, shell.trackColumn, store, { announcer });

  // Unified keyboard: T23's lane bindings first, then `?` toggles help and
  // Escape runs the cascade + clears the task selection + refocuses the
  // timeline (the legacy D9 fallback). The router is the T20 substrate.
  mountKeyRouter(window, [
    ...laneInteraction.keyBindings,
    // Toolbar + issues-rail accelerators (T33): `n`/`p` step the POI rail,
    // `g` focuses the goto-time input. After the lane bindings so a live
    // keyboard selection still owns arrows/Enter/Escape.
    ...shell.keyBindings,
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

  // Load chrome (T34): the drop zone / file picker / drag-drop / URL loading
  // surfaces + the New File flow. It consumes the T16 worker pipeline
  // (loadTraceInWorker), registers its Escape surface in the cascade, and
  // auto-loads the boot `?trace=` components when present; otherwise it shows
  // the drop zone (the resting empty state, B1). Load failures surface as an
  // error toast (U4/B13).
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

  // Mirror the shareable state INTO the URL as it changes (#1), debounced.
  // Registered last so the boot-time restore above does not fight it; the
  // copy-link button flushes it first (beforeCopyLink) so a copy is current.
  urlBinding = bindViewStateToUrl(store, {
    slices: ["viewport", "selection", "uiPrefs"],
    project: projectViewerState,
    mirrorToQuery: mirrorViewerToQuery,
  });

  // Teardown hook for HMR / tests (not strictly needed in production).
  window.addEventListener("beforeunload", () => {
    urlBinding?.dispose();
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
 * Resolve the boot trace source from `?trace=` components. With none, the
 * urls are empty: the load chrome shows the drop zone and waits for a
 * drop / pick / demo instead of auto-loading (features/02 B1 - the drop zone
 * is the resting empty state; the demo is a link, not the default).
 */
function traceSource(search: string): TraceSource {
  const params = new URLSearchParams(search);
  const raw = params.getAll("trace").filter((u) => u.length > 0);
  if (raw.length === 0) return { urls: [], label: "" };
  // Resolve each ?trace= value against the ORIGIN ROOT before it reaches the
  // worker. On the legacy page (/viewer.html) a relative value like
  // "demo-trace.bin" is root-relative by construction, but the migrated entry
  // (/new/viewer.html) hands the fetch to a worker whose script URL is under
  // /assets/ (or /src/lib/trace/worker/ in dev). A bare relative value would
  // resolve against the WORKER url and fetch an HTML fallback, which the parser
  // rejects with "Invalid trace header" - the failure seen from the browser
  // page's "load demo" button (window.open("viewer.html?trace=demo-trace.bin")).
  // Root-relative ("/api/object?...") and absolute values pass through. Mirrors
  // the flamegraph page's resolveTraceUrls (src/pages/flamegraph/query.ts).
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
