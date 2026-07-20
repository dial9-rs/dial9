// Browser-page actions: the verbs the components dispatch. DOM writes
// become store updates that subscribed components render through the
// store scheduler. Text inputs stay DOM-owned; actions read them live.

// Leaf seam modules, NOT the lib barrels: the barrel indexes evaluate
// modules that import trace_analysis.js / trace_parser.js at init (they
// expect <script>-established globals), which this page must not load.
import {
  MAX_OPEN_BYTES,
  groupByHost,
  segmentSpan,
  segmentGaps,
  segmentsOverlapping,
  tileSegments,
  totalBytes,
} from "../../lib/canvas/heatmap.js";
import {
  addDiffCapture,
  chooseTarget,
  removeDiffSide,
  swapDiffCapture,
} from "../../lib/canvas/flamegraph_diff.js";
import { parseKey } from "../../lib/trace/keys.js";
import { isDateLayer } from "../../lib/trace/prefixes.js";
import { traceTitleParams } from "../../lib/trace/title.js";
import {
  encodeAggregationParams,
  encodeScope,
  scopeFromKeys,
} from "../../lib/trace/trace_scope.js";
import { apiFetch, sampleBucketKeys, type BrowseResponse } from "./api.js";
import type { BrowserEls } from "./dom.js";
import { dateToPickerStr, epochSeconds, pickerToDate, xToTime } from "./format.js";
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

/** px height of each host row in the heatmap. */
export const ROW_H = 26;

/**
 * Heatmap grouping key for an unknown-layout S3 key: the key's raw
 * directory path, so segments from the same directory share a row without
 * any guessed service/host labels. Falls back to the whole key for keys
 * with no directory.
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

  // On page load, automatically run the search once so the heatmap is
  // populated without an extra click.
  let autoSearched = false;

  function localTz(): boolean {
    return store.getState().ui.useLocalTz;
  }

  // Mirror the current page state into the URL, replacing the history entry
  // so a stream of actions doesn't stack up Back-button steps. A quick range
  // is stored relative (`last=N`); a manually-edited range as precise
  // epoch-second from/to.
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
      // Echo the assume-role ARN so the read stays shareable (it's not a
      // secret; static BYOC keys are never serialized). Only the role
      // transport carries one.
      roleArn: storedCreds && storedCreds.kind === "role" ? storedCreds.roleArn : "",
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
    history.replaceState(
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

  // Full data extent across segments.
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
          const sampleKeys = await sampleBucketKeys(bucket);
          status = sampleKeys.length > 0
            ? {
                visible: true,
                kind: "normal",
                text: "No traces found in this time range. Sample keys in this bucket:",
                sampleKeys,
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

      // Build normalized segments for the density timeline. A segment's
      // wall-clock span is [trace-start epoch, last_modified]; bytes are
      // spread uniformly across it when rendering density. Unknown-layout
      // keys group by their raw directory path; their filename epoch is
      // layout-independent, so time placement is unchanged.
      const segments: HeatmapSegment[] = allObjects
        .map((obj) => {
          const p = parseKey(obj.key);
          // Local traces carry no date/epoch in the key, so fall back to the
          // upload mtime for time placement (#627); S3 traces keep p.epoch.
          const mtime = epochSeconds(obj.last_modified);
          const start = p.epoch || mtime;
          const end = mtime || start;
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
      // plus the genuine coverage gaps between them.
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

  // Region auto-detection: resolve a (possibly cross-region) bucket's real
  // region via /api/credentials/check before any data endpoint is hit, and
  // persist it into the stored credentials. No-op without credentials.
  async function detectRegionForBucket(bucket: string): Promise<void> {
    if (!bucket || !window.Dial9Creds || !window.Dial9Creds.has()) return;
    try {
      const result = await window.Dial9Creds.check(bucket);
      if (result.ok && result.region) {
        const stored = window.Dial9Creds.get();
        if (stored && stored.region !== result.region) {
          // Patch the region in place (known now, no second round trip).
          // setRegion keeps the active transport's kind, so it works for an
          // assumed-role credential too - a static-only set() would throw
          // on the role path.
          window.Dial9Creds.setRegion(result.region);
          els.credsRegion.value = result.region;
          syncUrl();
        }
      }
    } catch {
      // Best-effort: on failure, leave the prior region in place - the data
      // request will surface the actual error (e.g. the 421 wrong-region).
    }
  }

  // Load-time auto-search. Renders are frame-deferred, so evaluate the
  // search-readiness formula from state rather than the live DOM
  // `search-btn.disabled` - identical semantics, no scheduler race.
  function autoSearch(): void {
    if (autoSearched) return;
    autoSearched = true;
    const waitingForPrefix =
      store.getState().config.serverHasPrefix && els.prefixInput.value.trim() === "";
    if (els.bucketInput.value.trim() && !waitingForPrefix) {
      void doTimeRangeSearch();
    }
  }

  // Re-run whichever search the user last triggered.
  function reRunCurrentSearch(): void {
    if (store.getState().ui.tab === "raw") {
      if (els.rawSearchInput.value.trim()) void doRawSearch();
    } else if (els.bucketInput.value.trim()) {
      void doTimeRangeSearch();
    }
  }

  // Wipe the browse pane back to its initial empty state; used when
  // credentials are cleared.
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

  // Zoom the displayed time domain to the pixel range [x0, x1]. Density
  // re-normalizes to the visible window on the repaint.
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

  // The bucket's region, stamped onto every link this page opens so the
  // opened tab's URL is self-contained (signs the right regional S3 endpoint
  // without inheriting a detected region). Source of truth is the credentials
  // store; the panel's region field is the fallback. "" when unknown (the
  // server then uses its default region).
  function currentRegion(): string {
    const stored = window.Dial9Creds ? window.Dial9Creds.get() : null;
    if (stored && stored.region) return stored.region;
    return els.credsRegion.value.trim() || "";
  }

  // The selection spanned too many hosts to name them all in the URL, so the
  // scope degraded to "all hosts in the time window". Warn that the opened
  // view may be broader than the literal box selection.
  function warnHostsDropped(): void {
    alert(
      "This selection spans too many hosts to put in the link, so the opened " +
        "view covers ALL hosts in the selected time window. Narrow the time " +
        "range or host selection for an exact match.",
    );
  }

  // Open the viewer on the selection. Carry it as a compact *scope*
  // (bucket/prefix/service/host-set + time window) rather than one trace=
  // component per file, so a large selection's URL stays under CloudFront's
  // 8192-byte cap and re-resolves in any browser (#589). Local mode is the
  // exception: buffer-style local key names carry nothing to derive a scope
  // from, so open those directly by key (#627).
  function viewSelected(): void {
    const keys = getSelectedKeys();
    if (keys.length === 0) return;

    const bucket = els.bucketInput.value.trim();

    if (store.getState().config.localMode) {
      const params = new URLSearchParams();
      for (const key of keys) {
        params.append("trace", "/api/object?key=" + encodeURIComponent(key));
      }
      window.open("viewer.html?" + params.toString(), "_blank");
      return;
    }

    // Structured metadata for the viewer title, plus the scope the viewer
    // re-lists files from. Unknown-layout keys don't leak shifted svc/host.
    const titleParams = traceTitleParams(keys, { localTz: localTz() });
    const sel = store.getState().browse.selection;
    const scope = scopeFromKeys(
      bucket,
      keys,
      sel ? sel.t0 : null,
      sel ? sel.t1 : null,
      currentRegion(),
    );
    // Raw mode passes no window, so scopeFromKeys derives it from the keys'
    // epochs; null means an unrecognized layout with nothing to scope.
    if (!scope) {
      alert(
        "Could not derive a time range from the selected files. Their key " +
          "names may use an unrecognized layout.",
      );
      return;
    }
    const { query, hostsDropped } = encodeScope(titleParams, scope);
    if (hostsDropped) warnHostsDropped();
    window.open(`viewer.html?${query}`, "_blank");
  }

  // Flamegraph button. Two modes: aggregation-enabled servers get the sampled
  // server-side refinement loop over the selection's scope (encoded in the
  // un-namespaced aggregation vocabulary, `api=1` selecting the demand-driven
  // path); otherwise the exact per-file decode over a compact `s_*` scope.
  // A captured A/B diff routes the button to the two-sided diff instead (#623).
  function viewCpuProfile(): void {
    const s = store.getState();
    if (s.diff.a && s.diff.b) {
      launchDiff("flamegraph");
      return;
    }
    const sel = s.browse.selection;
    if (!sel || !sel.keys.length) return;
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      alert("Bucket is required");
      return;
    }
    const scope = scopeFromKeys(bucket, sel.keys, sel.t0, sel.t1, currentRegion());
    if (!scope) return; // a box selection always carries a window, so unreachable

    if (s.config.aggregationEnabled) {
      const base = new URLSearchParams();
      base.set("api", "1");
      const { query, hostsDropped } = encodeAggregationParams(base, scope);
      if (hostsDropped) warnHostsDropped();
      window.open("flamegraph.html?" + query, "_blank");
      return;
    }

    // Exact mode: open the selected raw traces and decode client-side.
    const fgParams = traceTitleParams(sel.keys, { localTz: localTz() });
    const { query, hostsDropped } = encodeScope(fgParams, scope);
    if (hostsDropped) warnHostsDropped();
    window.open("flamegraph.html?" + query, "_blank");
  }

  // Tokio Stats button: the same aggregation scope the demand-driven
  // flamegraph uses (/api/tokio-stats reads the same param vocabulary). A
  // captured A/B diff routes to the two-sided diff instead (#623).
  function viewTokioStats(): void {
    const s = store.getState();
    if (s.diff.a && s.diff.b) {
      launchDiff("tokio");
      return;
    }
    const sel = s.browse.selection;
    if (!sel || !sel.keys.length) return;
    const bucket = els.bucketInput.value.trim();
    if (!bucket) {
      alert("Bucket is required");
      return;
    }
    const scope = scopeFromKeys(bucket, sel.keys, sel.t0, sel.t1, currentRegion());
    if (!scope) return; // window always present for a box selection
    const { query, hostsDropped } = encodeAggregationParams(new URLSearchParams(), scope);
    if (hostsDropped) warnHostsDropped();
    window.open("tokio_stats.html?" + query, "_blank");
  }

  // ── Differential comparison (A/B) ──
  // Two captured aggregate scopes launched into either viz via the shared
  // FlamegraphDiff scope-link codec (?diff=1&a=<b64>&b=<b64>), which both
  // flamegraph.html and tokio_stats.html consume. Held in the store only.

  // The current selection as an aggregate scope (bucket/prefix/service/
  // host-set + start_ns/end_ns), shared with the flamegraph/tokio buttons so
  // all three agree on how a box maps to a scope. Null when there's no usable
  // selection (or no bucket).
  function selectionScope(): URLSearchParams | null {
    const sel = store.getState().browse.selection;
    if (!sel || !sel.keys.length) return null;
    const bucket = els.bucketInput.value.trim();
    if (!bucket) return null;
    const scope = scopeFromKeys(bucket, sel.keys, sel.t0, sel.t1, currentRegion());
    if (!scope) return null;
    const { query, hostsDropped } = encodeAggregationParams(new URLSearchParams(), scope);
    if (hostsDropped) warnHostsDropped();
    return new URLSearchParams(query);
  }

  // Capture the current selection as one side (A first, then B) of a diff.
  function addToDiff(): void {
    const scope = selectionScope();
    if (!scope) {
      if (!els.bucketInput.value.trim()) alert("Bucket is required");
      return;
    }
    store.update("diff", addDiffCapture(store.getState().diff, scope));
  }

  function clearDiff(): void {
    store.update("diff", { a: null, b: null });
  }

  // Swap A and B (no-op unless both are set - the codec fills A first).
  function swapDiff(): void {
    store.update("diff", swapDiffCapture(store.getState().diff));
  }

  // Remove one side (clearing A promotes B to A so there's never a B-without-A).
  function clearDiffSide(side: "a" | "b"): void {
    store.update("diff", removeDiffSide(store.getState().diff, side));
  }

  // Launch the captured A/B diff in the chosen viz via the shared scope-link
  // codec, so flamegraph and tokio-stats receive the same two-scope encoding.
  function launchDiff(kind: "flamegraph" | "tokio"): void {
    const d = store.getState().diff;
    if (!d.a || !d.b) return;
    const { page, search } = chooseTarget(kind, { hasDiff: true, diffA: d.a, diffB: d.b });
    window.open(page + "?" + search, "_blank");
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
    addToDiff,
    clearDiff,
    swapDiff,
    clearDiffSide,
    launchDiff,
  };
}
