// Browser-page actions (T14): the verbs the components dispatch. Each one
// mirrors a block of the legacy index.html inline script (cited per
// function); the DOM writes the legacy code performed inline become store
// updates here, and the subscribed components render them (through the
// store scheduler). Text inputs stay DOM-owned; actions read them live,
// exactly like the legacy handlers did.

// Leaf seam modules, NOT the lib barrels: the barrel indexes evaluate
// modules that import trace_analysis.js / trace_parser.js at init (they
// expect <script>-established globals in a browser), and the legacy
// browser page never loaded the parser at all - its port must not either.
import {
  MAX_OPEN_BYTES,
  groupByHost,
  segmentSpan,
  segmentGaps,
  segmentsOverlapping,
  tileSegments,
  totalBytes,
} from "../../lib/canvas/heatmap.js";
import { extractPrefix, parseKey } from "../../lib/trace/keys.js";
import { objectTraceUrls } from "../../lib/trace/object-urls.js";
import { isDateLayer } from "../../lib/trace/prefixes.js";
import { traceTitleParams } from "../../lib/trace/title.js";
import { apiFetch, type BrowseResponse } from "./api.js";
import type { BrowserEls } from "./dom.js";
import { dateToPickerStr, pickerToDate, xToTime } from "./format.js";
import { sortRawRows, toRawRows } from "./raw-rows.js";
import type {
  BrowseObject,
  BrowserStore,
  HeatmapRow,
  HeatmapSegment,
  HeatmapSelection,
  StatusState,
  TimeDomain,
} from "./state.js";
import type { UrlStateFields } from "./globals.js";

/** px height of each host row in the heatmap (legacy ROW_H). */
export const ROW_H = 26;

/**
 * Heatmap grouping key for an unknown-layout S3 key (T15, I2 amendment):
 * the key's raw directory path, so segments from the same directory share
 * a row (at least the granularity the legacy positional grouping had)
 * without any guessed service/host labels. Falls back to the whole key
 * for keys with no directory.
 */
export function unknownGroupPath(key: string): string {
  const dir = key.split("/").slice(0, -1).join("/");
  return dir || key;
}

export interface BrowserActions {
  syncUrl(): void;
  setRestoring(on: boolean): void;
  mirrorPrefix(): void;
  setQuickRange(hours: number): void;
  clearQuickRange(): void;
  switchTab(tab: "browse" | "raw"): void;
  toggleTz(): void;
  doTimeRangeSearch(): Promise<void>;
  doRawSearch(): Promise<void>;
  discoverPrefixes(): Promise<void>;
  detectRegionForBucket(bucket: string): Promise<void>;
  autoSearch(): void;
  isAutoSearched(): boolean;
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
}

export function createActions(store: BrowserStore, els: BrowserEls): BrowserActions {
  // While restoring state from the URL on load, syncUrl() is suppressed: the
  // intermediate restore steps (tab switch, range set) would otherwise each
  // rewrite the URL and could drop fields not yet restored (legacy
  // `restoring`).
  let restoring = false;

  // On page load, automatically run the search once so the heatmap is
  // populated without an extra click (legacy `autoSearched`).
  let autoSearched = false;

  function localTz(): boolean {
    return store.getState().ui.useLocalTz;
  }

  // Mirror the current page state into the URL, replacing the history entry
  // so a stream of actions doesn't stack up Back-button steps. A quick range
  // is stored relative (`last=N`); a manually-edited range as precise
  // epoch-second from/to. Ported from legacy syncUrl (index.html:725-750).
  function syncUrl(): void {
    if (restoring) return;
    // The bucket's region rides in the URL (as aws_region) so a cross-region
    // bucket is reproducible from a shared link. The credentials store is
    // the source of truth; the secret keys are header-only, never here.
    const storedCreds = window.Dial9Creds ? window.Dial9Creds.get() : null;
    const s = store.getState();
    const state: UrlStateFields = {
      bucket: els.bucketInput.value.trim(),
      region: (storedCreds && storedCreds.region) || "",
      prefix: els.prefixInput.value.trim(),
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
    const qs = window.Dial9UrlState.serialize(state);
    // Keep the pathname explicit: a bare "?qs" would resolve against
    // <base href="/"> and rewrite this off-root page's path to "/". The
    // legacy page (served at the root) got the same result implicitly.
    history.replaceState(
      null,
      "",
      window.location.pathname + (qs ? "?" + qs : ""),
    );
  }

  function mirrorPrefix(): void {
    store.update("form", { prefix: els.prefixInput.value });
  }

  // Quick range buttons (legacy setQuickRange, index.html:1099-1114).
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

  // A manual edit turns the range into a precise/custom window (legacy
  // clearQuickRange, index.html:1119-1123).
  function clearQuickRange(): void {
    store.update("search", { quickRange: null });
    syncUrl();
  }

  // Tab switching (legacy switchTab, index.html:1128-1140). All the DOM
  // toggles (tab classes, view/actions visibility, selection count) render
  // from the ui slice.
  function switchTab(tab: "browse" | "raw"): void {
    store.update("ui", { tab });
    syncUrl();
  }

  // Full data extent across segments (from legacy renderHeatmap's domain
  // computation, index.html:1297-1303).
  function computeExtent(segments: readonly HeatmapSegment[]): TimeDomain {
    let tMin = Infinity;
    let tMax = -Infinity;
    for (const s of segments) {
      const span = segmentSpan(s);
      if (span.start < tMin) tMin = span.start;
      if (span.end > tMax) tMax = span.end;
    }
    if (!(tMax > tMin)) tMax = tMin + 1;
    return { tMin, tMax };
  }

  // The state transition of legacy renderHeatmap (index.html:1281-1330):
  // clear the selection, and either show the "no traces" status (no rows)
  // or reset the domain to the data extent and show the heatmap. The actual
  // painting is the browse-view component's render.
  function renderHeatmapState(): void {
    const b = store.getState().browse;
    if (!b.rows.length) {
      store.update("browse", {
        selection: null,
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
      selection: null,
      fullDomain: extent,
      domain: { ...extent },
      heatmapVisible: true,
      status: { ...b.status, visible: false },
    });
  }

  // Timezone toggle (legacy tz-btn handler, index.html:953-972).
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

    // Re-render current view (legacy: renderHeatmap() / renderRawTable()).
    const s = store.getState();
    if (s.ui.tab === "browse") {
      if (s.browse.rows.length) renderHeatmapState();
    } else {
      // Legacy re-ran renderRawTable(rawObjects): rebuilds the table (which
      // drops any checked rows) or, when empty, re-runs the sample-key
      // empty-state fetch.
      void renderRawResults(s.raw.objects);
    }
    syncUrl();
  }

  // Time-range search, Browse mode (legacy doTimeRangeSearch,
  // index.html:1151-1246). One GET /api/browse; the server owns the prefix
  // fan-out (#582) and returns merged objects plus a `truncated` flag.
  async function doTimeRangeSearch(): Promise<void> {
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      alert("Bucket is required");
      return;
    }

    const fromStr = els.rangeFrom.value;
    const toStr = els.rangeTo.value;
    if (!fromStr || !toStr) {
      alert("Select a time range");
      return;
    }

    const tz = localTz();
    const from = pickerToDate(fromStr, tz)!;
    const to = pickerToDate(toStr, tz)!;
    const fromEpoch = Math.floor(from.getTime() / 1000);
    const toEpoch = Math.floor(to.getTime() / 1000);
    // When the server has its own prefix it owns prefixing; otherwise pass
    // the user-entered key prefix. The server combines it with any default.
    const keyPrefix = !store.getState().config.serverHasPrefix
      ? els.prefixInput.value.trim()
      : "";

    store.update("browse", {
      status: { visible: true, kind: "normal", text: "Searching…", sampleKeys: null },
      warning: null,
      heatmapVisible: false,
      selection: null,
    });

    try {
      let url =
        `/api/browse?bucket=${encodeURIComponent(bucket)}` +
        `&from=${fromEpoch}&to=${toEpoch}`;
      if (keyPrefix) url += `&prefix=${encodeURIComponent(keyPrefix)}`;
      const resp = await apiFetch(url);
      if (!resp.ok) {
        const body = await resp.text().catch(() => "");
        throw new Error(`HTTP ${resp.status}${body ? ": " + body : ""}`);
      }
      const result = (await resp.json()) as BrowseResponse;
      let allObjects: BrowseObject[] = result.objects || [];

      // The server lists whole time buckets, so the first/last bucket can
      // include objects just outside the requested window. Trim to the
      // actual range.
      allObjects = allObjects.filter((obj) => {
        const p = parseKey(obj.key);
        if (!p.epoch) return true; // keep if we can't parse
        // Include if segment overlaps the range (last_modified as end proxy)
        const end = obj.last_modified
          ? new Date(obj.last_modified).getTime() / 1000
          : p.epoch;
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
          const sampleResp = await apiFetch(
            `/api/browse?bucket=${encodeURIComponent(bucket)}&from=${Math.floor(Date.now() / 1000) - 86400}&to=${Math.floor(Date.now() / 1000)}`,
          );
          const samples = sampleResp.ok
            ? ((await sampleResp.json()) as BrowseResponse).objects ?? []
            : [];
          if (samples.length > 0) {
            status = {
              visible: true,
              kind: "normal",
              text: "No traces found in this time range. Sample keys in this bucket:",
              sampleKeys: samples.slice(0, 5).map((o) => o.key),
            };
          } else {
            status = {
              visible: true,
              kind: "normal",
              text: "No traces found in this time range. Bucket appears empty.",
              sampleKeys: null,
            };
          }
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

      // Build normalized segments for the density timeline. A segment's
      // wall-clock span is [trace-start epoch, last_modified]; bytes are
      // spread uniformly across it when rendering density. Unknown-layout
      // keys (T15, I2 amendment) group by their raw directory path - the
      // browse view labels those rows with the raw path instead of the
      // legacy positionally shifted service/host (Finding 1); their
      // filename epoch is layout-independent, so time placement is
      // unchanged.
      const segments: HeatmapSegment[] = allObjects
        .map((obj) => {
          const p = parseKey(obj.key);
          const start = p.epoch;
          const end = obj.last_modified
            ? new Date(obj.last_modified).getTime() / 1000
            : start;
          const shared = { key: obj.key, size: obj.size, start, end };
          if (p.layout === "known") {
            return {
              ...shared,
              layout: "known" as const,
              service: p.service,
              host: p.host,
              bootId: p.bootId,
            };
          }
          return {
            ...shared,
            layout: "unknown" as const,
            service: "",
            host: unknownGroupPath(obj.key),
            bootId: "",
          };
        })
        .filter((s) => s.start > 0);

      // Precompute per-row density inputs once (reused across zoom/resize
      // redraws): segments tiled so upload-lag overlaps don't double-count,
      // plus the genuine coverage gaps between them (legacy renderHeatmap
      // stamped these onto each row).
      const rows: HeatmapRow[] = groupByHost(segments).map((row) => ({
        ...row,
        tiled: tileSegments(row.segments),
        gaps: segmentGaps(row.segments),
      }));

      store.update("browse", { segments, rows });
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

  // Raw search (legacy doRawSearch, index.html:1790-1812). Post-#582 this
  // is GET /api/browse with an implicit last-30-days window.
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
      // Legacy read `.objects` with no fallback; a missing field would have
      // thrown into the catch below, so keep that shape strict.
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

  // The state transition of legacy renderRawTable (index.html:1818-1866):
  // empty results -> status + async sample-key hint; results -> table
  // rebuild (renderEpoch bump; the raw-view component builds the rows).
  // Legacy's rebuild left every checkbox unchecked, so the selection mirror
  // resets with it. NOTE: the empty-state sample fetch uses plain fetch(),
  // not apiFetch - a legacy quirk (G7) preserved verbatim.
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
        const bucket = els.bucketInput.value.trim();
        const sampleResp = await fetch(
          `/api/browse?bucket=${encodeURIComponent(bucket)}&from=${Math.floor(Date.now() / 1000) - 86400}&to=${Math.floor(Date.now() / 1000)}`,
        );
        const samples = sampleResp.ok
          ? ((await sampleResp.json()) as BrowseResponse).objects ?? []
          : [];
        if (samples.length > 0) {
          statusPatch = {
            text: "No results found. Sample keys in this bucket:",
            sampleKeys: samples.slice(0, 5).map((o) => o.key),
          };
        } else {
          statusPatch = { text: "No results found. Bucket appears empty.", sampleKeys: null };
        }
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

  // Auto-discover prefixes (legacy discoverPrefixes, index.html:877-935).
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
      // Legacy cleared the suggestion chips right after parsing, before any
      // early return below.
      store.update("search", { suggestions: [], activeSuggestion: null });
      if (prefixes.length === 0) {
        store.update("search", { prefixPlaceholder: "(none found)" });
        return;
      }
      // Issue #471: when the root children are all date partitions
      // (YYYY-MM-DD/), the bucket has no key prefix - the trace data starts
      // directly at the date layer. Don't offer the dates as prefix
      // suggestions; the correct prefix is empty.
      if (!store.getState().config.serverHasPrefix && isDateLayer(prefixes)) {
        els.prefixInput.value = "";
        mirrorPrefix();
        store.update("search", { prefixPlaceholder: "(no prefix — dates at root)" });
        store.update("config", { serverHasPrefix: false });
        syncUrl();
        return;
      }
      // If there's exactly one prefix and the input is empty, auto-select it
      if (prefixes.length === 1 && !els.prefixInput.value) {
        els.prefixInput.value = prefixes[0]!.replace(/\/$/, "");
        mirrorPrefix();
        syncUrl();
      }
      const labels = prefixes.map((p) => p.replace(/\/$/, ""));
      const current = els.prefixInput.value;
      store.update("search", {
        prefixPlaceholder: "e.g. traces",
        suggestions: labels,
        // The active chip matches the legacy per-chip render check
        // `prefixInput.value === label`; later manual edits of the prefix
        // input deliberately do NOT move this highlight (legacy behavior).
        activeSuggestion: labels.includes(current) ? current : null,
      });
    } catch {
      store.update("search", {
        prefixPlaceholder: "discovery failed — enter manually",
      });
    }
  }

  // Region auto-detection (#607; legacy detectRegionForBucket,
  // index.html:856-874). Resolve a (possibly cross-region) bucket's real
  // region via /api/credentials/check before any data endpoint is hit, and
  // persist it into the stored credentials. No-op without credentials.
  async function detectRegionForBucket(bucket: string): Promise<void> {
    if (!bucket || !window.Dial9Creds || !window.Dial9Creds.has()) return;
    try {
      const result = await window.Dial9Creds.check(bucket);
      if (result.ok && result.region) {
        const stored = window.Dial9Creds.get();
        if (stored && stored.region !== result.region) {
          // Persist without a second round trip (region is known now).
          void window.Dial9Creds.set({
            ...stored,
            region: result.region,
            autoDetectRegion: false,
          });
          els.credsRegion.value = result.region;
          syncUrl();
        }
      }
    } catch {
      // Best-effort: on failure, leave the prior region in place - the data
      // request will surface the actual error (e.g. the 421 wrong-region).
    }
  }

  // Load-time auto-search (legacy autoSearch, index.html:837-843). Legacy
  // gated on the live DOM `search-btn.disabled`; renders are frame-deferred
  // here, so evaluate the same readiness formula (updateSearchReady) from
  // state instead - identical semantics, no scheduler race.
  function autoSearch(): void {
    if (autoSearched) return;
    autoSearched = true;
    const waitingForPrefix =
      store.getState().config.serverHasPrefix && els.prefixInput.value.trim() === "";
    if (els.bucketInput.value.trim() && !waitingForPrefix) {
      void doTimeRangeSearch();
    }
  }

  // Re-run whichever search the user last triggered (legacy
  // reRunCurrentSearch, index.html:690-696).
  function reRunCurrentSearch(): void {
    if (store.getState().ui.tab === "raw") {
      if (els.rawSearchInput.value.trim()) void doRawSearch();
    } else if (els.bucketInput.value.trim()) {
      void doTimeRangeSearch();
    }
  }

  // Wipe the browse pane back to its initial empty state; used when
  // credentials are cleared (legacy resetBrowsePane, index.html:1547-1559).
  function resetBrowsePane(): void {
    store.update("browse", {
      segments: [],
      rows: [],
      domain: null,
      fullDomain: null,
      selection: null,
      heatmapVisible: false,
      status: {
        visible: true,
        kind: "normal",
        text: "Select a time range and click Search to find traces.",
        sampleKeys: null,
      },
    });
  }

  function canvasWidth(): number {
    return (
      els.heatmapCanvas.clientWidth || parseFloat(els.heatmapCanvas.style.width) || 1
    );
  }

  // Zoom the displayed time domain to the pixel range [x0, x1] (legacy
  // zoomToX, index.html:1529-1541). Density re-normalizes to the visible
  // window on the repaint.
  function zoomToX(x0: number, x1: number): void {
    const b = store.getState().browse;
    if (!b.domain || x1 - x0 < 4) return;
    const W = canvasWidth();
    const { tMin, tMax } = b.domain;
    const t0 = xToTime(x0, tMin, tMax, W);
    const t1 = xToTime(x1, tMin, tMax, W);
    if (!(t1 > t0)) return;
    store.update("browse", { domain: { tMin: t0, tMax: t1 }, selection: null });
  }

  // Restore the full data extent (legacy resetHeatmapZoom,
  // index.html:1561-1569; no-op if not currently zoomed).
  function resetHeatmapZoom(): void {
    const b = store.getState().browse;
    if (!b.fullDomain) return;
    const { tMin, tMax } = b.fullDomain;
    if (b.domain && b.domain.tMin === tMin && b.domain.tMax === tMax) return;
    store.update("browse", { domain: { tMin, tMax }, selection: null });
  }

  // Single-click: select the one segment under the cursor (legacy
  // selectSegmentAt, index.html:1599-1615).
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

  // Drag-select region (legacy finalizeSelection, index.html:1617-1642).
  // Opening fetches WHOLE segment files, so the selection snaps to the
  // actual [min start, max end] of the covered files.
  function finalizeSelection(x0: number, x1: number, y0: number, y1: number): void {
    const b = store.getState().browse;
    if (!b.domain || x1 - x0 < 3) {
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

  // (legacy setHeatmapSelection, index.html:1644-1659; the rect, label
  // highlights and selection count all render from the slice.)
  function setHeatmapSelection(sel: HeatmapSelection | null): void {
    store.update("browse", { selection: sel });
  }

  // Raw-mode Select All / Deselect All (legacy rawSelectAll,
  // index.html:1872-1876). The checkboxes are DOM-owned; mirror the result
  // into the raw slice for the count render.
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

  // Selected keys (legacy getSelectedKeys, index.html:1879-1886). Raw mode
  // returns keys in TABLE ROW ORDER exactly as the legacy
  // `.raw-cb:checked` DOM walk did - since T15's G8 amendment the row
  // order follows the active column sort (default: epoch ascending).
  function getSelectedKeys(): string[] {
    const s = store.getState();
    if (s.ui.tab === "browse") {
      return s.browse.selection ? [...s.browse.selection.keys] : [];
    }
    return sortRawRows(toRawRows(s.raw.objects), s.raw.sort)
      .map((r) => r.obj.key)
      .filter((key) => s.raw.selected.has(key));
  }

  // Open the viewer on the selection (legacy viewSelected,
  // index.html:1924-1936).
  function viewSelected(): void {
    const keys = getSelectedKeys();
    if (keys.length === 0) return;

    const bucket = els.bucketInput.value.trim();

    // Pass structured metadata for the viewer title, plus one trace=
    // component per file (downloaded in parallel + gunzipped client-side).
    // T15 (I2 amendment): lib/trace/title.ts - unknown-layout keys no
    // longer leak shifted svc/host params into the title.
    const titleParams = traceTitleParams(keys, { localTz: localTz() });
    for (const url of objectTraceUrls(bucket, keys)) titleParams.append("trace", url);

    window.open(`viewer.html?${titleParams.toString()}`, "_blank");
  }

  // Flamegraph button (legacy viewCpuProfile, index.html:1728-1768). Two
  // modes since #570: aggregation-enabled servers get the sampled
  // server-side refinement loop over the selection's scope; otherwise the
  // exact per-key trace= set.
  function viewCpuProfile(): void {
    const s = store.getState();
    const sel = s.browse.selection;
    if (!sel || !sel.keys.length) return;
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      alert("Bucket is required");
      return;
    }
    const keys = sel.keys;
    if (s.config.aggregationEnabled) {
      // T15 (I2 amendment): scope params come from KNOWN layouts only -
      // the legacy path fed positionally shifted fields (svc=host-0) into
      // the aggregation scope, filtering on wrong names.
      const known = keys.map((k) => parseKey(k)).filter((p) => p.layout === "known");
      const services = [...new Set(known.map((p) => p.service).filter(Boolean))];
      const hosts = [...new Set(known.map((p) => p.host).filter(Boolean))];
      const fgParams = new URLSearchParams();
      fgParams.set("api", "1");
      // Pass the bucket so the flamegraph endpoint knows which bucket to
      // aggregate from (required for bring-your-own-credentials mode).
      if (bucket) fgParams.set("bucket", bucket);
      // Extract the key prefix from the first selected key (everything
      // before the date segment). Authoritative regardless of whether the
      // prefix came from the server or the user's input (I8).
      const pfx = extractPrefix(keys[0]!);
      if (pfx) fgParams.set("prefix", pfx);
      // The loop's Scope takes a single service; a box almost always spans
      // one service. If several, pass the first and let host-set + time do
      // the narrowing.
      if (services.length) fgParams.set("service", services[0]!);
      for (const h of hosts) fgParams.append("host", h); // repeatable host set
      // Selection times are epoch SECONDS; the loop wants epoch ns.
      if (sel.t0 != null) fgParams.set("start_ns", String(Math.round(sel.t0 * 1e9)));
      if (sel.t1 != null) fgParams.set("end_ns", String(Math.round(sel.t1 * 1e9)));
      window.open("flamegraph.html?" + fgParams.toString(), "_blank");
      return;
    }

    // Exact mode: open the selected raw traces and decode client-side.
    const fgParams = traceTitleParams(keys, { localTz: localTz() });
    for (const url of objectTraceUrls(bucket, keys)) fgParams.append("trace", url);
    window.open("flamegraph.html?" + fgParams.toString(), "_blank");
  }

  // Tokio Stats button (#570; legacy viewTokioStats, index.html:1770-1787).
  function viewTokioStats(): void {
    const s = store.getState();
    const sel = s.browse.selection;
    if (!sel || !sel.keys.length) return;
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      alert("Bucket is required");
      return;
    }
    const keys = sel.keys;
    // T15 (I2 amendment): known layouts only, as in viewCpuProfile above.
    const known = keys.map((k) => parseKey(k)).filter((p) => p.layout === "known");
    const services = [...new Set(known.map((p) => p.service).filter(Boolean))];
    const hosts = [...new Set(known.map((p) => p.host).filter(Boolean))];
    const hp = new URLSearchParams();
    if (bucket) hp.set("bucket", bucket);
    const pfx = extractPrefix(keys[0]!);
    if (pfx) hp.set("prefix", pfx);
    if (services.length) hp.set("service", services[0]!);
    for (const h of hosts) hp.append("host", h);
    if (sel.t0 != null) hp.set("start_ns", String(Math.round(sel.t0 * 1e9)));
    if (sel.t1 != null) hp.set("end_ns", String(Math.round(sel.t1 * 1e9)));
    window.open("tokio_stats.html?" + hp.toString(), "_blank");
  }

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
    detectRegionForBucket,
    autoSearch,
    isAutoSearched: () => autoSearched,
    reRunCurrentSearch,
    resetBrowsePane,
    zoomToX,
    resetHeatmapZoom,
    selectSegmentAt,
    finalizeSelection,
    setHeatmapSelection,
    rawSelectAll,
    syncRawSelectionFromDom,
    getSelectedKeys,
    viewSelected,
    viewCpuProfile,
    viewTokioStats,
  };
}
