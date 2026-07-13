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
import { loadTraceInWorker } from "../../lib/trace/index.js";
import { mountKeyRouter } from "../../lib/interact/index.js";
import { getAnnouncer } from "../../lib/interact/announce.js";
import { createViewerStore } from "./store.js";
import { createEscCascade, ESC_PRIORITY } from "./esc-cascade.js";
import { mountViewerHelp } from "./help.js";
import { createToasts } from "./toasts.js";
import { mountShell } from "./shell.js";
import { mountInspector } from "./inspector.js";
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
  const shell = mountShell(root, store, {
    toggleHelp: () => help.toggle(),
    sourceLabel: source.label,
  });
  const toasts = createToasts(shell.toastRegion);

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
  const inspector = mountInspector(shell.inspectorRegion, store, { esc });

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

  // Bootstrap load: `?trace=` components, else the bundled demo trace. This
  // is a stand-in for T34's drop-zone/URL pipeline - enough to render the
  // shell on real data (the DoD: shell renders on the demo trace).
  const load = loadTraceInWorker(store, source.urls);
  load.done.catch((err: unknown) => {
    if (err instanceof DOMException && err.name === "AbortError") return;
    // One-time load failure surfaced to the user via an error toast (U4);
    // not a loop, so no rate-limiting needed.
    const detail = err instanceof Error ? err.message : String(err);
    toasts.show({
      id: "load-error",
      type: "error",
      message: `Could not load ${source.label}: ${detail}`,
    });
    console.error("viewer: trace load failed", err);
  });

  // Teardown hook for HMR / tests (not strictly needed in production).
  window.addEventListener("beforeunload", () => {
    load.abort();
    laneInteraction.dispose();
    overlay.dispose();
    inspector.dispose();
    lanes.dispose();
    shell.dispose();
    help.dispose();
  });
}

interface TraceSource {
  urls: readonly string[];
  label: string;
}

/** Resolve the trace source: `?trace=` components, else the demo trace. */
function traceSource(search: string): TraceSource {
  const params = new URLSearchParams(search);
  const traceUrls = params.getAll("trace").filter((u) => u.length > 0);
  if (traceUrls.length > 0) {
    const first = traceUrls[0] ?? "trace";
    return { urls: traceUrls, label: lastPathSegment(first) };
  }
  return { urls: ["/demo-trace.bin"], label: "demo-trace.bin" };
}

function lastPathSegment(url: string): string {
  const noQuery = url.split("?")[0] ?? url;
  const parts = noQuery.split("/").filter((p) => p.length > 0);
  return parts[parts.length - 1] ?? url;
}
