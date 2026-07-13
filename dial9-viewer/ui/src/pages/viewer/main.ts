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
import { createEscCascade, ESC_PRIORITY } from "./esc-cascade.js";
import { mountViewerHelp } from "./help.js";
import { createToasts } from "./toasts.js";
import { mountShell } from "./shell.js";
import { mountLoadChrome } from "./load-chrome.js";
import { mountLanes } from "../../components/canvas/lanes/index.js";
import { mountOverlay } from "../../components/overlay/index.js";
import { mountLaneInteraction } from "./lane-interaction.js";
import { initViewportFromTrace } from "./viewport-init.js";

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
  const notify = (message: string): void => {
    toastsRef?.show({ id: "t33-seam", type: "info", message });
  };
  const shell = mountShell(root, store, {
    toggleHelp: () => help.toggle(),
    sourceLabel: () => loadChrome?.currentLabel() ?? source.label,
    onNewFile: () => loadChrome?.requestNewFile(),
    onOpenAnalysis: (kind) =>
      notify(`${analysisName(kind)} analysis opens in the inspector (region analyses, T32).`),
    onSetRange: () =>
      notify("Set Range re-parses the loaded trace to the current view (load chrome, T34)."),
    onClearRange: () => notify("Clear Range restores the full trace (load chrome, T34)."),
  });
  const toasts = createToasts(shell.toastRegion);
  toastsRef = toasts;

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

  // Initialize the viewport from the trace the moment it loads. Registered
  // BEFORE the lane interaction so its zoom-history baseline records the
  // fitted view (both subscribe to `trace`; order = registration order).
  initViewportFromTrace(store);

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

  // Teardown hook for HMR / tests (not strictly needed in production).
  window.addEventListener("beforeunload", () => {
    loadChrome?.dispose();
    laneInteraction.dispose();
    overlay.dispose();
    lanes.dispose();
    shell.dispose();
    help.dispose();
  });
}

/** Human name for an analysis-button seam message (T33 -> T32). */
function analysisName(kind: "cpu" | "blocking" | "heap"): string {
  switch (kind) {
    case "cpu":
      return "CPU flamegraph";
    case "blocking":
      return "Blocking-calls";
    case "heap":
      return "Heap";
  }
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
  const traceUrls = params.getAll("trace").filter((u) => u.length > 0);
  if (traceUrls.length > 0) {
    const first = traceUrls[0] ?? "trace";
    return { urls: traceUrls, label: lastPathSegment(first) };
  }
  return { urls: [], label: "" };
}

function lastPathSegment(url: string): string {
  const noQuery = url.split("?")[0] ?? url;
  const parts = noQuery.split("/").filter((p) => p.length > 0);
  return parts[parts.length - 1] ?? url;
}
