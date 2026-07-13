// The exact (client-decode) mode of the flamegraph page: fetches, gunzips, and
// parses one or more `?trace=` components, builds worker spans to attach spawn
// locations, filters samples to the optional `?start=/?end=` window, and
// renders the flamegraph widget with zoom state synced to the URL. The fetch +
// gunzip + parse run inside the Web Worker load pipeline (loadTraceInWorker) so
// the multi-second parse does not stall the page.

import {
  EVENT_TYPES,
  attachCpuSamples,
  buildWorkerSpans,
  canStreamDecode,
  Dial9Creds,
  loadTraceInWorker,
} from "../../lib/trace/index.js";
import type { ParsedTrace, TraceSliceStore } from "../../lib/trace/index.js";
import { createFlamegraph, filterCpuSamples } from "../../lib/canvas/index.js";
import { mountCopyLink } from "../../lib/url/index.js";
import type { PageEls } from "./dom.js";
import { closeHelpOnEscape, mountFlamegraphKeys } from "./fg-keys.js";
import type { FgKeys } from "./fg-keys.js";
import { loadingLabel, resolveTraceUrls } from "./query.js";
import { createFgUrlSync, restoreZoomFromUrl } from "./view-state.js";

export async function runExactMode(
  params: URLSearchParams,
  els: PageEls
): Promise<void> {
  // Mounted before the load so a share is possible even from the loading/error
  // states. The zoom->URL sync exists only once the widget renders, hence the
  // late-bound flush.
  let flushUrlState: (() => void) | null = null;
  mountCopyLink(els.headerEl, {
    beforeCopy: () => {
      flushUrlState?.();
    },
  });

  // `trace` is repeatable - each value is a separate (possibly gzipped)
  // component to fetch and concatenate.
  const rawTraceUrls = params.getAll("trace");
  const startParam = params.get("start");
  const endParam = params.get("end");
  const startNs = startParam != null ? Number(startParam) : null;
  const endNs = endParam != null ? Number(endParam) : null;

  if (!rawTraceUrls.length) {
    els.showError(
      "No trace URL provided. Use ?trace=<url>&start=<ns>&end=<ns> or ?api=1&data_dir=..."
    );
    return;
  }

  // Relative components resolve against the origin root (see resolveTraceUrls);
  // the label fallback keeps using the raw first value.
  const traceUrls = resolveTraceUrls(rawTraceUrls, window.location.origin);

  // Same-origin credential headers: resolved on the main thread; the core's
  // isSameOrigin withholding still applies per URL in the worker.
  const credHeaders = Dial9Creds.headers();

  // Loading label: the worker picks stream vs buffered itself with the same
  // canStreamDecode rule, so the main-thread answer sets the initial label;
  // progress messages keep it accurate afterwards.
  const setLoadLabel = (mode: "stream" | "buffered", phase: "fetching" | "parsing"): void => {
    els.loadingEl.textContent = loadingLabel(mode, phase, rawTraceUrls.length);
  };
  setLoadLabel(canStreamDecode() ? "stream" : "buffered", "fetching");

  // The worker pipeline writes the parsed trace into a `trace` slice on
  // completion (its contract); this page consumes the awaited result
  // directly, so a minimal one-slot slice stands in for a full store.
  const traceSlice: { trace: ParsedTrace | null } = { trace: null };
  const store: TraceSliceStore = {
    update(_slice, patch): void {
      traceSlice.trace = patch.trace;
    },
  };

  let trace: ParsedTrace;
  try {
    const load = loadTraceInWorker(store, traceUrls, {
      headers: credHeaders,
      onProgress: (p): void => {
        setLoadLabel(p.mode, p.phase);
      },
    });
    ({ trace } = await load.done);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    // HTTP 401 with no stored credentials -> the specific hint.
    if (/HTTP 401/.test(message) && !Dial9Creds.has()) {
      els.showError(
        "This trace requires AWS credentials. Open it from the dial9 " +
          "home page after applying your credentials."
      );
    } else {
      els.showError("Failed to load trace: " + message);
    }
    return;
  }

  // Build worker spans and attach CPU samples for spawnLoc.
  try {
    els.loadingEl.innerHTML = '<div class="spinner"></div>Analyzing\u2026';
    const evts = trace.events;
    const wSet = new Set<number>();
    for (const e of evts) {
      if (
        e.eventType !== EVENT_TYPES.QueueSample &&
        e.eventType !== EVENT_TYPES.WakeEvent
      ) {
        wSet.add(e.workerId);
      }
    }
    const workerIds = [...wSet].sort((a, b) => a - b);
    const lastEvent = evts.at(-1);
    const maxTs = lastEvent !== undefined ? lastEvent.timestamp : 0;
    const spanResult = buildWorkerSpans(evts, workerIds, maxTs);
    if (trace.cpuSamples.length > 0) {
      attachCpuSamples(trace.cpuSamples, spanResult.workerSpans);
    }
  } catch (err) {
    // Non-fatal: spawnLoc just won't be available.
    console.warn("Failed to attach spawn locations:", err);
  }

  // Filter to the selected time range, falling back to all samples if the URL
  // timestamps don't align with the parsed trace.
  let allSamples = filterCpuSamples(trace.cpuSamples, startNs, endNs);
  let timeRangeMatched = allSamples.length > 0;
  if (!timeRangeMatched) {
    const unfiltered = filterCpuSamples(trace.cpuSamples, null, null);
    if (unfiltered.length > 0 && (startNs != null || endNs != null)) {
      allSamples = unfiltered;
      console.warn(
        "Time range filter matched 0 samples \u2014 showing all",
        unfiltered.length,
        "samples"
      );
    } else {
      els.showError("No CPU samples found in the specified time range.");
      return;
    }
  }

  // Update title. Prefer the structured svc/host metadata; otherwise fall back
  // to the trace filename.
  const svc = params.get("svc");
  const host = params.get("host");
  const segs = params.get("segs");
  const fromTime = params.get("from");
  const toTime = params.get("to");
  const firstRaw = rawTraceUrls[0] ?? "";
  const label = svc
    ? svc + (host ? ` @ ${host}` : "")
    : firstRaw.split("/").pop() || "trace";
  const durMs =
    startNs != null && endNs != null
      ? ((endNs - startNs) / 1e6).toFixed(2)
      : null;
  document.title = `Flamegraph \u2014 ${label}` + (durMs ? ` (${durMs}ms)` : "");
  els.titleEl.textContent = `Flamegraph \u2014 ${label}`;
  const titleBits = [`${allSamples.length} samples`];
  if (segs) titleBits.push(`${segs} segment${segs !== "1" ? "s" : ""}`);
  if (fromTime) {
    titleBits.push(
      toTime && toTime !== fromTime ? `${fromTime} \u2192 ${toTime}` : fromTime
    );
  }
  if (durMs) titleBits.push(`${durMs}ms selected`);
  if (!timeRangeMatched) {
    titleBits.push("full trace \u2014 selected region could not be reproduced");
  }
  els.statsEl.textContent = titleBits.join(" \u00b7 ");

  els.loadingEl.classList.add("hidden");
  els.containerEl.style.display = "flex";

  // Zoom -> URL sync: one debounced replaceState per zoom burst, touching the
  // two zoom query params (every other param survives) and carrying the
  // versioned view-state hash. The lazy getZoomPath closure resolves the widget
  // created on the next line.
  const urlSync = createFgUrlSync(() => fg.getZoomPath());
  flushUrlState = () => {
    urlSync.flush();
  };

  // Unified keys: mounted after the URL zoom restore below so the landed state
  // is the undo baseline; late-bound here because the widget's onZoomChange
  // must also record zoom history for `z`.
  let keys: FgKeys | null = null;
  const fg = createFlamegraph(els.containerEl, () => {
    keys?.recordZoom();
    urlSync.onZoomChange();
  });
  fg.setData(allSamples, trace.callframeSymbols, {
    exportTitle: `Flamegraph \u2014 ${label}`,
    runtimeWorkers: trace.runtimeWorkers,
  });

  // Restore zoom from URL (only if the time-range filter succeeded, otherwise
  // the tree differs). Hash state wins per field over the query params;
  // restoring writes nothing back (see view-state.ts).
  restoreZoomFromUrl(
    { search: window.location.search, hash: window.location.hash },
    fg,
    timeRangeMatched,
  );

  // Unified keys: `?` help, `f` fit, `z` zoom-undo; the widget's own `/` /
  // Ctrl+F / Escape bindings stay untouched. f/z settle the zoom URL sync
  // through onZoomMutated.
  keys = mountFlamegraphKeys({
    fg,
    containerEl: els.containerEl,
    onZoomMutated: urlSync.onZoomChange,
  });

  window.addEventListener("resize", () => fg.resize());

  // Escape cascade: an open unified help overlay closes first (ahead of the
  // widget cascade; no-op when closed).
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      if (keys !== null && closeHelpOnEscape(keys)) return;
      fg.handleEscape();
    }
  });
}
