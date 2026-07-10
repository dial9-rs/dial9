// src/pages/flamegraph/exact-mode.ts - the exact (client-decode) mode of
// the standalone flamegraph page (T13): a behavior-preserving port of the
// legacy inline bootstrap's load path (flamegraph.html:504-643;
// features/03 sections A-C + M/N wiring). One or more `?trace=`
// components are fetched + gunzipped + parsed, worker spans are built to
// attach spawn locations, samples are filtered to the optional
// `?start=/?end=` window, and the frozen flamegraph widget renders the
// result with zoom state synced to the URL.
//
// Mechanism difference from legacy (deliberate, T16): the fetch + gunzip
// + parse run inside the Web Worker load pipeline
// (lib/trace loadTraceInWorker) instead of on the main thread, so the
// multi-second parse no longer stalls the page. The observable contract
// is unchanged: same stream/buffered selection (made in the worker), same
// loading-phase labels (F9, derived from the worker's progress messages),
// same error surface (error name/message cross the boundary intact, so
// the HTTP-401 credentials hint still fires), same analysis + rendering.

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
import type { PageEls } from "./dom.js";
import { loadingLabel, resolveTraceUrls } from "./query.js";

export async function runExactMode(
  params: URLSearchParams,
  els: PageEls
): Promise<void> {
  // `trace` is repeatable - each value is a separate (possibly gzipped)
  // component to fetch and concatenate (F1).
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

  // Relative components resolve against the origin root, where the
  // canonical page lives - see query.ts resolveTraceUrls. The label
  // fallback (F24) keeps using the raw first value, like legacy.
  const traceUrls = resolveTraceUrls(rawTraceUrls, window.location.origin);

  // Same-origin credential headers (F8): resolved on the main thread, the
  // core's isSameOrigin withholding still applies per URL in the worker.
  const credHeaders = Dial9Creds.headers();

  // Loading label (F9): the worker picks stream vs buffered itself with
  // the same canStreamDecode rule, so the main-thread answer sets the
  // initial label; progress messages keep it accurate afterwards.
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
    // HTTP 401 with no stored credentials -> the specific hint (F12).
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

  // Process: build worker spans and attach CPU samples for spawnLoc
  // (F14-F18).
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
    // Non-fatal: spawnLoc just won't be available (F18).
    console.warn("Failed to attach spawn locations:", err);
  }

  // Filter to the selected time range, fall back to all samples if the
  // URL timestamps don't align with the parsed trace (F19-F21).
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

  // Update title. Prefer the structured svc/host metadata passed by the
  // S3 browser; otherwise fall back to the trace filename (F22-F30).
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

  // Zoom -> URL sync (F147-F153): replaceState of the rebuilt query,
  // touching only the two zoom params so every other param survives.
  function updateUrlZoom(): void {
    const p = new URLSearchParams(window.location.search);
    const z = fg.getZoomPath();
    if (z.worker.length > 0) p.set("worker-zoom", z.worker.join("\t"));
    else p.delete("worker-zoom");
    if (z.offworker.length > 0) p.set("offworker-zoom", z.offworker.join("\t"));
    else p.delete("offworker-zoom");
    const newUrl = window.location.pathname + "?" + p.toString();
    window.history.replaceState(null, "", newUrl);
  }

  const fg = createFlamegraph(els.containerEl, updateUrlZoom);
  fg.setData(allSamples, trace.callframeSymbols, {
    exportTitle: `Flamegraph \u2014 ${label}`,
    runtimeWorkers: trace.runtimeWorkers,
  });

  // Restore zoom from URL (only if the time-range filter succeeded,
  // otherwise the tree differs - F151).
  if (timeRangeMatched) {
    const wz = params.get("worker-zoom");
    const oz = params.get("offworker-zoom");
    if (wz) fg.zoomToPath("worker", wz.split("\t"));
    if (oz) fg.zoomToPath("offworker", oz.split("\t"));
  }

  // Responsive resize (F155).
  window.addEventListener("resize", () => fg.resize());

  // Escape cascade (F157).
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      fg.handleEscape();
    }
  });
}
