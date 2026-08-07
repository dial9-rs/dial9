// Browser-page actions: the verbs the components dispatch. DOM writes
// become store updates that subscribed components render through the
// store scheduler. Text inputs stay DOM-owned; actions read them live.

// Leaf seam modules, NOT the lib barrels: the barrel indexes evaluate
// modules that import trace_analysis.js / trace_parser.js at init (they
// expect <script>-established globals), which this page must not load.
import {
  segmentSpan,
  segmentsOverlapping,
  totalBytes,
} from "../../lib/canvas/heatmap.js";
import { parseKey } from "../../lib/trace/keys.js";
import { Dial9Creds } from "../../lib/trace/creds.js";
import {
  isLiteralConfigured,
  makeSourceScope,
  type SourceScope,
} from "../../lib/trace/source-scope.js";
import { isDateLayer, preferredPrefix } from "../../lib/trace/prefixes.js";
import { DRAG_INTENT_PX } from "../../lib/interact/pointer.js";
import {
  apiFetch,
  sampleBucketKeys,
  type BrowseResponse,
  type ServicesResponse,
} from "./api.js";
import {
  buildBrowseUrl,
  buildServicesUrl,
  canSelectService,
  resolveRequestedService,
  resolveServiceSelection,
} from "./browse-query.js";
import { createDiffActions } from "./diff-actions.js";
import type { BrowserEls } from "./dom.js";
import { dateToPickerStr, epochSeconds, pickerToDate, xToTime } from "./format.js";
import { createOpenLinks } from "./open-links.js";
import { sortRawRows, toRawRows } from "./raw-rows.js";
import { computeExtent, toRows, toSegments } from "./segments.js";
import type {
  BrowseObject,
  BrowserStore,
  HeatmapSegment,
  HeatmapSelection,
  StatusState,
} from "./state.js";
import type { UrlStateFields } from "./globals.js";

/** px height of each host row in the heatmap. */
export const ROW_H = 26;

export interface BrowserActions {
  syncUrl(historyMode?: "replace" | "push"): void;
  setRestoring(on: boolean): void;
  mirrorPrefix(): void;
  setQuickRange(hours: number): void;
  clearQuickRange(): void;
  switchTab(tab: "browse" | "raw"): void;
  toggleTz(): void;
  doTimeRangeSearch(): Promise<void>;
  doRawSearch(): Promise<void>;
  discoverPrefixes(): Promise<void>;
  discoverServices(): Promise<void>;
  submitBrowseSearch(): void;
  clearBrowseNoService(): void;
  selectService(service: string, historyMode?: "replace" | "push"): void;
  detectRegionForBucket(bucket: string): Promise<void>;
  canRerunCurrentSearch(): boolean;
  reRunCurrentSearch(): void;
  resetBrowsePane(): void;
  zoomToX(x0: number, x1: number): void;
  resetHeatmapZoom(): void;
  selectSegmentAt(x: number, y: number): void;
  finalizeSelection(x0: number, x1: number, y0: number, y1: number): void;
  setHeatmapSelection(sel: HeatmapSelection | null): void;
  rawSelectAll(checked: boolean): void;
  syncRawSelectionFromDom(): void;
  getSelectedKeys(): string[];
  viewSelected(): void;
  viewCpuProfile(): void;
  viewTokioStats(): void;
  viewSpanExplorer(): void;
  addToDiff(): void;
  clearDiff(): void;
  swapDiff(): void;
  clearDiffSide(side: "a" | "b"): void;
  launchDiff(kind: "flamegraph" | "tokio"): void;
}

export function createActions(store: BrowserStore, els: BrowserEls): BrowserActions {
  // While restoring state from the URL on load, syncUrl() is suppressed: the
  // intermediate restore steps (tab switch, range set) would otherwise each
  // rewrite the URL and could drop fields not yet restored.
  let restoring = false;

  let serviceDiscoveryGeneration = 0;
  let browseGeneration = 0;

  function localTz(): boolean {
    return store.getState().ui.useLocalTz;
  }

  function currentSource(): SourceScope {
    const source = store.getState().source;
    return makeSourceScope(
      els.bucketInput.value.trim(),
      els.credsRegion?.value.trim() || source.region,
      source.credentials,
    );
  }

  // Mirror the current page state into the URL, replacing the history entry
  // so a stream of actions doesn't stack up Back-button steps. A quick range
  // is stored relative (`last=N`); a manually-edited range as precise
  // epoch-second from/to.
  function syncUrl(historyMode: "replace" | "push" = "replace"): void {
    if (restoring) return;
    const source = currentSource();
    const previousSource = store.getState().source;
    if (
      source.bucket !== previousSource.bucket ||
      source.region !== previousSource.region ||
      source.credentials !== previousSource.credentials
    ) {
      store.update("source", source);
    }
    const s = store.getState();
    const state: UrlStateFields = {
      bucket: source.bucket,
      region: source.region,
      credentialMode: source.credentials.kind,
      ...(source.credentials.kind === "role"
        ? { roleArn: source.credentials.roleArn }
        : {}),
      prefix: els.prefixInput.value.trim(),
      service: els.serviceInput.value.trim(),
      tab: s.ui.tab,
      tz: s.ui.useLocalTz ? "local" : "utc",
      q: els.rawSearchInput.value.trim(),
    };
    if (s.search.quickRange) {
      state.last = s.search.quickRange;
    } else {
      const fromDate = pickerToDate(els.rangeFrom.value, s.ui.useLocalTz);
      const toDate = pickerToDate(els.rangeTo.value, s.ui.useLocalTz);
      if (fromDate) state.from = Math.floor(fromDate.getTime() / 1000);
      if (toDate) state.to = Math.floor(toDate.getTime() / 1000);
    }
    let qs = window.Dial9UrlState.serialize(state);
    // A page-load `bucket_filter=` override must survive URL syncs, but
    // Dial9UrlState doesn't know the param - re-append it here. An empty
    // override ("no filtering") is meaningful and serialized too.
    const filterOverride = s.config.bucketFilterOverride;
    if (filterOverride != null) {
      qs += (qs ? "&" : "") + "bucket_filter=" + encodeURIComponent(filterOverride);
    }
    // Keep the pathname explicit: a bare "?qs" would resolve against
    // <base href="/"> and rewrite this off-root page's path to "/".
    history[historyMode === "push" ? "pushState" : "replaceState"](
      null,
      "",
      window.location.pathname + (qs ? "?" + qs : ""),
    );
  }

  function mirrorPrefix(): void {
    store.update("form", { prefix: els.prefixInput.value });
  }

  function setQuickRange(hours: number): void {
    const now = new Date();
    const from = new Date(now.getTime() - hours * 3600 * 1000);
    els.rangeFrom.value = dateToPickerStr(from, localTz());
    els.rangeTo.value = dateToPickerStr(now, localTz());
    // Remember the relative window so the URL stays relative ("last N hours
    // from now") rather than freezing these computed timestamps. The button
    // highlight renders from this slice.
    store.update("search", { quickRange: hours });
    syncUrl();
  }

  // A manual edit turns the range into a precise/custom window.
  function clearQuickRange(): void {
    store.update("search", { quickRange: null });
    syncUrl();
  }

  // Tab switching: all the DOM toggles (tab classes, view/actions
  // visibility, selection count) render from the ui slice.
  function switchTab(tab: "browse" | "raw"): void {
    store.update("ui", { tab });
    syncUrl();
  }

  // Clear the selection, and either show the "no traces" status (no rows)
  // or reset the domain to the data extent and show the heatmap. The actual
  // painting is the browse-view component's render.
  //
  // `preserveSelection` keeps the current selection and zoom (#644): a TZ
  // toggle only reformats labels, so the box the user drew must survive it.
  function renderHeatmapState(opts?: { preserveSelection?: boolean }): void {
    const preserve = opts?.preserveSelection ?? false;
    const b = store.getState().browse;
    if (!b.rows.length) {
      store.update("browse", {
        selection: preserve ? b.selection : null,
        heatmapVisible: false,
        status: {
          visible: true,
          kind: "normal",
          text: "No traces found in this time range.",
          sampleKeys: null,
        },
      });
      return;
    }
    const extent = computeExtent(b.segments);
    store.update("browse", {
      selection: preserve ? b.selection : null,
      fullDomain: extent,
      domain: preserve && b.domain ? b.domain : { ...extent },
      heatmapVisible: true,
      status: { ...b.status, visible: false },
    });
  }

  function toggleTz(): void {
    const wasLocal = localTz();
    // Read current picker values in the OLD tz mode before toggling
    const fromDate = pickerToDate(els.rangeFrom.value, wasLocal);
    const toDate = pickerToDate(els.rangeTo.value, wasLocal);

    const nowLocal = !wasLocal;
    store.update("ui", { useLocalTz: nowLocal });

    // Re-write picker values in the NEW tz mode
    if (fromDate) els.rangeFrom.value = dateToPickerStr(fromDate, nowLocal);
    if (toDate) els.rangeTo.value = dateToPickerStr(toDate, nowLocal);

    // Re-render the current view. A TZ toggle is display-only, so keep the
    // selection and zoom the user set (#644) - only the labels reformat.
    const s = store.getState();
    if (s.ui.tab === "browse") {
      if (s.browse.rows.length) renderHeatmapState({ preserveSelection: true });
    } else {
      // Rebuild the raw table (dropping any checked rows) or, when empty,
      // re-run the sample-key empty-state fetch.
      void renderRawResults(s.raw.objects);
    }
    syncUrl();
  }

  // Time-range search, Browse mode. One GET /api/browse; the server owns
  // the prefix fan-out and returns merged objects plus a `truncated` flag.
  async function doTimeRangeSearch(): Promise<void> {
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      alert("Bucket is required");
      return;
    }

    const tz = localTz();
    // pickerToDate also returns null for a non-empty but unparseable value, so
    // an emptiness check alone would let NaN epochs reach the request URL.
    const from = pickerToDate(els.rangeFrom.value, tz);
    const to = pickerToDate(els.rangeTo.value, tz);
    if (!from || !to) {
      alert("Select a time range");
      return;
    }

    const fromEpoch = Math.floor(from.getTime() / 1000);
    const toEpoch = Math.floor(to.getTime() / 1000);
    // When the server has its own prefix it owns prefixing; otherwise pass
    // the user-entered key prefix. The server combines it with any default.
    const keyPrefix = !store.getState().config.serverHasPrefix
      ? els.prefixInput.value.trim()
      : "";
    const service = els.serviceInput.value.trim();
    if (!service) {
      await discoverServices();
      return;
    }
    const generation = ++browseGeneration;

    store.update("browse", {
      status: { visible: true, kind: "normal", text: "Searching…", sampleKeys: null },
      warning: null,
      heatmapVisible: false,
      selection: null,
    });

    try {
      const url = buildBrowseUrl({
        bucket,
        from: fromEpoch,
        to: toEpoch,
        prefix: keyPrefix,
        service,
      });
      const resp = await apiFetch(url);
      if (!resp.ok) {
        const body = await resp.text().catch(() => "");
        throw new Error(`HTTP ${resp.status}${body ? ": " + body : ""}`);
      }
      const result = (await resp.json()) as BrowseResponse;
      if (generation !== browseGeneration) return;
      let allObjects: BrowseObject[] = result.objects || [];

      // The server lists whole time buckets, so the first/last bucket can
      // include objects just outside the requested window. Trim to the
      // actual range.
      allObjects = allObjects.filter((obj) => {
        const p = parseKey(obj.key);
        if (!p.epoch) return true; // keep if we can't parse
        // Include if segment overlaps the range (last_modified as end proxy).
        // epochSeconds handles both numeric-epoch (local dir) and ISO-8601 (S3).
        const end = epochSeconds(obj.last_modified) || p.epoch;
        return p.epoch <= toEpoch && end >= fromEpoch;
      });

      // Surface a capped result so it's never mistaken for missing data.
      store.update("browse", {
        warning: result.truncated
          ? "Some traces were omitted: this time range exceeded the listing limit. " +
            "Narrow the window or focus on a shorter span to see everything."
          : null,
      });

      if (allObjects.length === 0) {
        store.update("browse", {
          status: { ...store.getState().browse.status, kind: "normal" },
          rows: [],
          heatmapVisible: false,
        });
        // Show sample keys to help the user understand the bucket layout
        let status: StatusState;
        try {
          const sampleKeys = await sampleBucketKeys(bucket, { service });
          if (generation !== browseGeneration) return;
          status = sampleKeys.length > 0
            ? {
                visible: true,
                kind: "normal",
                text: "No traces found in this time range. Sample keys in this bucket:",
                sampleKeys,
              }
            : service
              ? {
                  visible: true,
                  kind: "normal",
                  text: "No traces found for this service in this time range.",
                  sampleKeys: null,
                }
            : {
                visible: true,
                kind: "normal",
                text: "No traces found in this time range. Bucket appears empty.",
                sampleKeys: null,
              };
        } catch {
          status = {
            visible: true,
            kind: "normal",
            text: "No traces found in this time range.",
            sampleKeys: null,
          };
        }
        store.update("browse", { status });
        return;
      }

      const segments = toSegments(allObjects);
      store.update("browse", { segments, rows: toRows(segments) });
      renderHeatmapState();
    } catch (err) {
      store.update("browse", {
        status: {
          visible: true,
          kind: "error",
          text: "Error: " + (err instanceof Error ? err.message : String(err)),
          sampleKeys: null,
        },
      });
    }
  }

  // Raw search: GET /api/browse with an implicit last-30-days window.
  async function doRawSearch(): Promise<void> {
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      alert("Bucket is required");
      return;
    }

    const q = els.rawSearchInput.value.trim();
    const url = `/api/browse?bucket=${encodeURIComponent(bucket)}&prefix=${encodeURIComponent(q)}&from=${Math.floor(Date.now() / 1000) - 30 * 86400}&to=${Math.floor(Date.now() / 1000)}`;

    store.update("raw", {
      status: { visible: true, kind: "normal", text: "Searching…", sampleKeys: null },
      tableVisible: false,
    });

    try {
      const resp = await apiFetch(url);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}: ${await resp.text()}`);
      // Read `.objects` with no fallback: a missing field throws into the
      // catch below, keeping the shape strict.
      const objects = ((await resp.json()) as { objects: BrowseObject[] }).objects;
      store.update("raw", { objects });
      await renderRawResults(objects);
    } catch (err) {
      store.update("raw", {
        status: {
          visible: true,
          kind: "error",
          text: "Error: " + (err instanceof Error ? err.message : String(err)),
          sampleKeys: null,
        },
      });
    }
  }

  // Empty results -> status + async sample-key hint; results -> table
  // rebuild (renderEpoch bump; the raw-view component builds the rows).
  // The rebuild leaves every checkbox unchecked, so the selection mirror
  // resets with it.
  async function renderRawResults(objects: readonly BrowseObject[]): Promise<void> {
    if (objects.length === 0) {
      const prev = store.getState().raw.status;
      store.update("raw", {
        status: { ...prev, visible: true },
        tableVisible: false,
        selected: new Set<string>(),
        renderEpoch: store.getState().raw.renderEpoch + 1,
      });
      let statusPatch: Partial<StatusState>;
      try {
        const sampleKeys = await sampleBucketKeys(els.bucketInput.value.trim());
        statusPatch = sampleKeys.length > 0
          ? { text: "No results found. Sample keys in this bucket:", sampleKeys }
          : { text: "No results found. Bucket appears empty.", sampleKeys: null };
      } catch {
        statusPatch = { text: "No results found.", sampleKeys: null };
      }
      store.update("raw", {
        status: { ...store.getState().raw.status, ...statusPatch },
      });
      return;
    }

    const prev = store.getState().raw;
    store.update("raw", {
      status: { ...prev.status, visible: false },
      tableVisible: true,
      selected: new Set<string>(),
      renderEpoch: prev.renderEpoch + 1,
    });
  }

  async function discoverPrefixes(): Promise<void> {
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      store.update("search", { prefixPlaceholder: "e.g. traces" });
      return;
    }
    try {
      const resp = await apiFetch(`/api/prefixes?bucket=${encodeURIComponent(bucket)}`);
      if (!resp.ok) {
        const body = await resp.text().catch(() => "");
        console.warn(`prefix discovery failed: HTTP ${resp.status}${body ? ": " + body : ""}`);
        store.update("search", {
          prefixPlaceholder: "discovery failed — enter manually",
        });
        return;
      }
      const prefixes = (await resp.json()) as string[];
      // Clear the suggestion chips right after parsing, before any early
      // return below.
      store.update("search", { suggestions: [], activeSuggestion: null });
      if (prefixes.length === 0) {
        store.update("search", { prefixPlaceholder: "(none found)" });
        return;
      }
      // When the root children are all date partitions (YYYY-MM-DD/), the
      // bucket has no key prefix - the trace data starts directly at the
      // date layer. Don't offer the dates as prefix suggestions; the
      // correct prefix is empty.
      if (!store.getState().config.serverHasPrefix && isDateLayer(prefixes)) {
        els.prefixInput.value = "";
        mirrorPrefix();
        store.update("search", { prefixPlaceholder: "(no prefix — dates at root)" });
        store.update("config", { serverHasPrefix: false });
        syncUrl();
        return;
      }
      // Pre-select a prefix when the input is empty: `dial9-traces` if the
      // bucket offers it (the conventional default), otherwise the sole
      // prefix. With several non-default prefixes we leave it to the user.
      if (!els.prefixInput.value) {
        const preferred = preferredPrefix(prefixes);
        if (preferred) {
          els.prefixInput.value = preferred;
          mirrorPrefix();
          syncUrl();
        }
      }
      const labels = prefixes.map((p) => p.replace(/\/$/, ""));
      const current = els.prefixInput.value;
      store.update("search", {
        prefixPlaceholder: "e.g. traces",
        suggestions: labels,
        // Later manual edits of the prefix input deliberately do NOT move
        // this highlight.
        activeSuggestion: labels.includes(current) ? current : null,
      });
    } catch {
      store.update("search", {
        prefixPlaceholder: "discovery failed — enter manually",
      });
    }
  }

  // Explicit user gesture (Search button / Enter). Validates with visible
  // feedback before delegating to discovery, which otherwise returns silently
  // on a missing bucket or unparseable range (it also runs from background
  // chains that must not alert). Restores the pre-refactor alert behavior.
  function submitBrowseSearch(): void {
    if (!els.bucketInput.value.trim()) {
      alert("Bucket is required");
      return;
    }
    // pickerToDate returns null for a non-empty but unparseable value too, so
    // an emptiness check alone would let a bad range slip through silently.
    const tz = localTz();
    if (!pickerToDate(els.rangeFrom.value, tz) || !pickerToDate(els.rangeTo.value, tz)) {
      alert("Select a time range");
      return;
    }
    void discoverServices();
  }

  async function discoverServices(): Promise<void> {
    const generation = ++serviceDiscoveryGeneration;
    browseGeneration++;
    const bucket = els.bucketInput.value.trim();
    const waitingForPrefix =
      store.getState().config.serverHasPrefix && els.prefixInput.value.trim() === "";
    if (!bucket || waitingForPrefix) {
      store.update("browse", {
        services: [],
        serviceMetadata: [],
        activeService: null,
        serviceDiscovery: "idle",
      });
      return;
    }

    const tz = localTz();
    const from = pickerToDate(els.rangeFrom.value, tz);
    const to = pickerToDate(els.rangeTo.value, tz);
    if (!from || !to) return;

    const fromEpoch = Math.floor(from.getTime() / 1000);
    const toEpoch = Math.floor(to.getTime() / 1000);
    const keyPrefix = !store.getState().config.serverHasPrefix
      ? els.prefixInput.value.trim()
      : "";
    const requested = els.serviceInput.value.trim();
    const requestedSelection = resolveRequestedService(requested);
    if (requestedSelection) {
      store.update("browse", {
        services: [requested],
        serviceMetadata: [],
        activeService: requestedSelection.active,
        serviceDiscovery: "ready",
        segments: [],
        rows: [],
        domain: null,
        fullDomain: null,
        selection: null,
        heatmapVisible: false,
        warning: null,
        status: {
          visible: true,
          kind: "normal",
          text: "Loading service…",
          sampleKeys: null,
        },
      });
      syncUrl();
      await doTimeRangeSearch();
      return;
    }

    store.update("browse", {
      services: [],
      serviceMetadata: [],
      activeService: null,
      serviceDiscovery: "loading",
      segments: [],
      rows: [],
      domain: null,
      fullDomain: null,
      selection: null,
      heatmapVisible: false,
      warning: null,
      status: {
        visible: true,
        kind: "normal",
        text: "Finding services…",
        sampleKeys: null,
      },
    });

    try {
      const resp = await apiFetch(
        buildServicesUrl({
          bucket,
          from: fromEpoch,
          to: toEpoch,
          prefix: keyPrefix,
        }),
      );
      if (!resp.ok) {
        const body = await resp.text().catch(() => "");
        throw new Error(`HTTP ${resp.status}${body ? ": " + body : ""}`);
      }
      const result = (await resp.json()) as ServicesResponse;
      if (generation !== serviceDiscoveryGeneration) return;

      const services = result.services;
      const serviceMetadata = result.service_metadata ?? [];
      const selection = resolveServiceSelection(services, requested);
      els.serviceInput.value = selection.active ?? "";
      store.update("browse", {
        services,
        serviceMetadata,
        activeService: selection.active,
        serviceDiscovery: "ready",
        warning: result.truncated
          ? "Service discovery reached its listing limit. Some services may be omitted."
          : null,
        status: {
          visible: true,
          kind: "normal",
          text:
            services.length === 0
              ? "No services found in this time range."
              : selection.shouldLoad
                ? "Loading service…"
                : "Choose a service to browse its traces.",
          sampleKeys: null,
        },
      });
      syncUrl();
      if (selection.shouldLoad) await doTimeRangeSearch();
    } catch (err) {
      if (generation !== serviceDiscoveryGeneration) return;
      store.update("browse", {
        serviceDiscovery: "error",
        status: {
          visible: true,
          kind: "error",
          text: "Error: " + (err instanceof Error ? err.message : String(err)),
          sampleKeys: null,
        },
      });
    }
  }

  function selectService(
    service: string,
    historyMode: "replace" | "push" = "push",
  ): void {
    const browse = store.getState().browse;
    const requested = service.trim();
    const fromHistory = historyMode === "replace";
    if (!canSelectService(browse.services, requested, fromHistory)) return;
    // An explicit tab/history selection supersedes any slower discovery that
    // was started before it. Its response must not overwrite this service.
    serviceDiscoveryGeneration++;
    const knownService = browse.services.includes(requested);
    // Tab clicks must name a discovered service. History restoration is
    // different: direct loading deliberately leaves a one-service list, so a
    // Back/Forward entry may validly name another service not in that list.
    if (browse.activeService === requested && browse.rows.length > 0) return;
    els.serviceInput.value = requested;
    store.update("browse", {
      services: knownService ? browse.services : [requested],
      serviceMetadata: knownService ? browse.serviceMetadata : [],
      activeService: requested,
    });
    syncUrl(historyMode);
    void doTimeRangeSearch();
  }

  // Region auto-detection: resolve a (possibly cross-region) bucket's real
  // region via /api/credentials/check before any data endpoint is hit, and
  // persist it into the stored credentials. No-op without credentials.
  async function detectRegionForBucket(bucket: string): Promise<void> {
    const source = currentSource();
    const configured =
      source.credentials.kind === "role" || isLiteralConfigured(source.credentials);
    if (!bucket || !configured) return;
    try {
      const result = await Dial9Creds.check(bucket);
      if (result.ok && result.region && source.region !== result.region) {
        Dial9Creds.setRegion(result.region);
        els.credsRegion.value = result.region;
        store.update("source", { ...source, region: result.region });
        syncUrl();
      }
    } catch {
      // Best-effort: on failure, leave the prior region in place - the data
      // request will surface the actual error (e.g. the 421 wrong-region).
    }
  }

  function canRerunCurrentSearch(): boolean {
    if (store.getState().ui.tab === "raw") {
      return els.rawSearchInput.value.trim() !== "";
    }
    return (
      store.getState().browse.activeService !== null &&
      els.bucketInput.value.trim() !== ""
    );
  }

  // Re-run whichever search the user last triggered.
  function reRunCurrentSearch(): void {
    if (!canRerunCurrentSearch()) return;
    if (store.getState().ui.tab === "raw") {
      void doRawSearch();
    } else {
      void doTimeRangeSearch();
    }
  }

  // Wipe the browse pane back to its initial empty state; used when
  // credentials are cleared.
  function resetBrowsePane(): void {
    serviceDiscoveryGeneration++;
    browseGeneration++;
    els.serviceInput.value = "";
    store.update("browse", {
      segments: [],
      rows: [],
      services: [],
      serviceMetadata: [],
      activeService: null,
      serviceDiscovery: "idle",
      domain: null,
      fullDomain: null,
      selection: null,
      heatmapVisible: false,
      status: {
        visible: true,
        kind: "normal",
        text: "Select a bucket to find services.",
        sampleKeys: null,
      },
    });
    syncUrl();
  }

  // Back/Forward landed on a history entry with no service. Clear the browse
  // pane to the "pick a service" state. Bumping browseGeneration invalidates
  // any in-flight doTimeRangeSearch so its late response can't repopulate the
  // pane we just cleared (every other reset path bumps it too).
  function clearBrowseNoService(): void {
    browseGeneration++;
    els.serviceInput.value = "";
    store.update("browse", {
      activeService: null,
      segments: [],
      rows: [],
      domain: null,
      fullDomain: null,
      selection: null,
      heatmapVisible: false,
      status: {
        visible: true,
        kind: "normal",
        text: "Choose a service to browse its traces.",
        sampleKeys: null,
      },
    });
  }

  function canvasWidth(): number {
    return (
      els.heatmapCanvas.clientWidth || parseFloat(els.heatmapCanvas.style.width) || 1
    );
  }

  // Zoom the displayed time domain to the pixel range [x0, x1]. Density
  // re-normalizes to the visible window on the repaint.
  function zoomToX(x0: number, x1: number): void {
    const b = store.getState().browse;
    if (!b.domain || x1 - x0 <= DRAG_INTENT_PX) return;
    const W = canvasWidth();
    const { tMin, tMax } = b.domain;
    const t0 = xToTime(x0, tMin, tMax, W);
    const t1 = xToTime(x1, tMin, tMax, W);
    if (!(t1 > t0)) return;
    store.update("browse", { domain: { tMin: t0, tMax: t1 }, selection: null });
  }

  // Restore the full data extent; no-op if not currently zoomed.
  function resetHeatmapZoom(): void {
    const b = store.getState().browse;
    if (!b.fullDomain) return;
    const { tMin, tMax } = b.fullDomain;
    if (b.domain && b.domain.tMin === tMin && b.domain.tMax === tMax) return;
    store.update("browse", { domain: { tMin, tMax }, selection: null });
  }

  // Single-click: select the one segment under the cursor.
  function selectSegmentAt(x: number, y: number): void {
    const b = store.getState().browse;
    if (!b.domain) return;
    const r = Math.floor(y / ROW_H);
    if (r < 0 || r >= b.rows.length) {
      setHeatmapSelection(null);
      return;
    }
    const W = canvasWidth();
    const { tMin, tMax } = b.domain;
    const t = xToTime(x, tMin, tMax, W);
    const hits = segmentsOverlapping(b.rows[r]!.segments, t, t);
    if (!hits.length) {
      setHeatmapSelection(null);
      return;
    }
    // If multiple segments cover the instant, prefer the one whose start is
    // nearest (most specific) to the click.
    hits.sort((a, b2) => Math.abs(a.start - t) - Math.abs(b2.start - t));
    const seg = hits[0]!;
    const span = segmentSpan(seg);
    setHeatmapSelection({
      keys: [seg.key],
      bytes: seg.size,
      t0: span.start,
      t1: span.end,
      rows: [r, r],
    });
  }

  // Drag-select region. Opening fetches WHOLE segment files, so the
  // selection snaps to the actual [min start, max end] of the covered files.
  function finalizeSelection(x0: number, x1: number, y0: number, y1: number): void {
    const b = store.getState().browse;
    if (!b.domain || x1 - x0 <= DRAG_INTENT_PX) {
      setHeatmapSelection(null);
      return;
    }
    const W = canvasWidth();
    const { tMin, tMax } = b.domain;
    const dragT0 = xToTime(x0, tMin, tMax, W);
    const dragT1 = xToTime(x1, tMin, tMax, W);
    const r0 = Math.max(0, Math.floor(y0 / ROW_H));
    const r1 = Math.min(b.rows.length - 1, Math.floor((y1 - 0.001) / ROW_H));
    const segs: HeatmapSegment[] = [];
    for (let r = r0; r <= r1; r++) {
      for (const s of segmentsOverlapping(b.rows[r]!.segments, dragT0, dragT1)) {
        segs.push(s);
      }
    }
    if (!segs.length) {
      setHeatmapSelection(null);
      return;
    }
    let t0 = Infinity;
    let t1 = -Infinity;
    for (const s of segs) {
      const span = segmentSpan(s);
      if (span.start < t0) t0 = span.start;
      if (span.end > t1) t1 = span.end;
    }
    setHeatmapSelection({
      keys: segs.map((s) => s.key),
      bytes: totalBytes(segs),
      t0,
      t1,
      rows: [r0, r1],
    });
  }

  // The rect, label highlights and selection count all render from the slice.
  function setHeatmapSelection(sel: HeatmapSelection | null): void {
    store.update("browse", { selection: sel });
  }

  // Raw-mode Select All / Deselect All. The checkboxes are DOM-owned;
  // mirror the result into the raw slice for the count render.
  function rawSelectAll(checked: boolean): void {
    els.rawBody
      .querySelectorAll<HTMLInputElement>(".raw-cb")
      .forEach((cb) => {
        cb.checked = checked;
      });
    els.rawSelectAll.checked = checked;
    syncRawSelectionFromDom();
  }

  /** Rebuild the raw-selection mirror from the live checkbox state. */
  function syncRawSelectionFromDom(): void {
    const selected = new Set<string>();
    els.rawBody
      .querySelectorAll<HTMLInputElement>(".raw-cb:checked")
      .forEach((cb) => {
        if (cb.dataset["key"] != null) selected.add(cb.dataset["key"]);
      });
    store.update("raw", { selected });
  }

  // Selected keys. Raw mode returns keys in TABLE ROW ORDER following the
  // active column sort (default: epoch ascending).
  function getSelectedKeys(): string[] {
    const s = store.getState();
    if (s.ui.tab === "browse") {
      return s.browse.selection ? [...s.browse.selection.keys] : [];
    }
    return sortRawRows(toRawRows(s.raw.objects), s.raw.sort)
      .map((r) => r.obj.key)
      .filter((key) => s.raw.selected.has(key));
  }

  // Deep links (viewer / flamegraph / tokio-stats) and the A/B diff verbs
  // live in their own modules; they are wired together here because a
  // captured diff takes over the flamegraph and tokio buttons (#623) while
  // a diff capture reads the same selection scope those buttons build.
  const diffActions = createDiffActions(store, els);
  const openLinks = createOpenLinks({
    store,
    els,
    getSelectedKeys,
    launchDiff: diffActions.launchDiff,
  });

  return {
    syncUrl,
    setRestoring: (on: boolean) => {
      restoring = on;
    },
    mirrorPrefix,
    setQuickRange,
    clearQuickRange,
    switchTab,
    toggleTz,
    doTimeRangeSearch,
    doRawSearch,
    discoverPrefixes,
    discoverServices,
    submitBrowseSearch,
    selectService,
    detectRegionForBucket,
    canRerunCurrentSearch,
    reRunCurrentSearch,
    resetBrowsePane,
    clearBrowseNoService,
    zoomToX,
    resetHeatmapZoom,
    selectSegmentAt,
    finalizeSelection,
    setHeatmapSelection,
    rawSelectAll,
    syncRawSelectionFromDom,
    getSelectedKeys,
    ...openLinks,
    ...diffActions,
  };
}
