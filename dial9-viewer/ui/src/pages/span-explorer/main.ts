// The Span Explorer page entry.
//
// Two sources feed the same catalog:
//   RAW  (`?trace=`)  - the browser fetches the trace, then POSTs its bytes to
//                       /api/span-stats for one server-side Rust decode.
//   AGGREGATE (`?api=1` / `?bucket=` / `?data_dir=`) - /api/span-stats streams
//                       server-computed statistics over SSE.
//
// Three stream modes share one socket shape (see buildApiUrl):
//   "replace"   - a fresh scope; the catalog is rebuilt from the snapshot.
//   "refine"    - the same scope, deeper: snapshots below the visible baseline
//                 are ignored so the page never momentarily shrinks.
//   "exemplars" - the selected type's exemplars only, reading already-folded
//                 spans parts. Parses no raw files, so it never refines.

import {
  Dial9Creds,
  Dial9Session,
  addAttrFilter,
  applyToCreds,
  classifyExemplarSnapshot,
  completeExemplarRefresh,
  exemplarRequestMatches,
  formatCoverageBadge,
  hasAttrFilter,
  isSourceShareable,
  mergeSelectedExemplarSnapshot,
  nextMaxFiles,
  nsToPickerUtc,
  openSse,
  pageTitle,
  parseAttrFilterParams,
  pickerUtcToNs,
  refinementWorkDepth,
  removeAttrFilter,
  shouldAdoptCatalogSnapshot,
  sourceScopeFromStored,
  toUrlSourceScope,
} from "../../lib/trace/index.js";
import type {
  AttrFilter,
  Coverage,
  DurationBand,
  SpanExplorerState,
  SpanStatsResponse,
  SpanTypeStats,
  StreamMode,
} from "../../lib/trace/index.js";
import { mountBrowserLink } from "../../lib/url/index.js";
import { pageEls } from "./dom.js";
import {
  renderCatalog,
  nextSort,
  type CatalogSort,
  type SortKey,
} from "./catalog.js";
import { renderDetail } from "./detail.js";
import { SpanTailAnalysis } from "./tail-analysis.js";
import { renderFilterBar } from "./filters.js";
import {
  retainMatchedHistogramPair,
  type MatchedHistogramPair,
} from "./comparison.js";
import {
  buildApiUrl,
  buildBrowserQuery,
  exemplarScopeKey,
  isAggregateMode,
  readScope,
  readTailView,
  type TailViewState,
  type ViewState,
} from "./scope.js";
import {
  loadOverrides,
  saveOverrides,
  setOverride,
  type ColumnOverrides,
} from "./columns.js";
import {
  fetchRawTraceBytes,
  rawStatsSummary,
  requestRawSpanStats,
} from "./raw.js";

const els = pageEls();
const tailAnalysis = new SpanTailAnalysis();

// URL params are read ONCE: the load scope is fixed for the page's lifetime.
const params = new URLSearchParams(window.location.search);
mountBrowserLink(els.header, window.location.search);
const scope = readScope(params, sourceScopeFromStored("", Dial9Creds.get()));
const aggregate = isAggregateMode(params, scope);
const rawMode = scope.trace != null;

document.title = pageTitle("Span Explorer", scope.service);

applyToCreds(scope.source, Dial9Creds);
// Keep the static node and ID for the private userscript's page marker, but hide
// built-in sharing whenever literal credentials are active.
els.btnCopyLink.style.display = isSourceShareable(scope.source) ? "" : "none";

// ── Mutable page state ──

let spanTypes: SpanTypeStats[] = [];
let selectedUid = params.get("span_type_uid");
let analysisView = readTailView(params);
let catalogExpanded = false;
let band: DurationBand = {
  min_ns:
    params.get("min_span_ns") != null
      ? Number(params.get("min_span_ns"))
      : null,
  max_ns:
    params.get("max_span_ns") != null
      ? Number(params.get("max_span_ns"))
      : null,
};
let attrFilters: readonly AttrFilter[] = parseAttrFilterParams(
  params.getAll("attr"),
);
let maxFiles: number | null =
  params.get("max_files") != null ? Number(params.get("max_files")) : null;
let lastCoverage: Coverage | null = null;
let sort: CatalogSort = { key: "count", ascending: false };
let overrides: ColumnOverrides = loadOverrides();

// Attribute constraints produce a second, selected-type-only histogram. This
// state never replaces the unfiltered catalog or its selected baseline type.
let comparisonSpanType: SpanTypeStats | null = null;
let comparisonCoverage: Coverage | null = null;
let comparisonPending = false;
let comparisonHasSnapshot = false;
let comparisonError: string | null = null;
let matchedHistogramPair: MatchedHistogramPair | null = null;

// Which duration scope each type's cached exemplars were produced under, so a
// reselect can tell a stale set from a valid one.
const exemplarScopeByUid = new Map<string, string>();
let exemplarRefreshPending = false;
let exemplarPreviewAvailable = false;

// Stream state. A monotonic token retires callbacks from a superseded stream.
let streamToken = 0;
let abortCtl: AbortController | null = null;
let activeStreamMode: StreamMode | null = null;

// The comparison owns a separate stream so it cannot abort, hide, or otherwise
// perturb the baseline catalog stream.
let comparisonStreamToken = 0;
let comparisonAbortCtl: AbortController | null = null;
let comparisonStreaming = false;
let activeComparisonKey: string | null = null;
let comparisonRefreshAfterBaseline = false;

if (params.get("start_ns"))
  els.fStart.value = nsToPickerUtc(params.get("start_ns"));
if (params.get("end_ns")) els.fEnd.value = nsToPickerUtc(params.get("end_ns"));

function view(): ViewState {
  return {
    startNs: pickerUtcToNs(els.fStart.value),
    endNs: pickerUtcToNs(els.fEnd.value),
    selectedUid,
    bandMinNs: band.min_ns,
    bandMaxNs: band.max_ns,
    attrFilters,
    maxFiles,
  };
}

/** The scope the flamegraph deep links are built against; null in raw mode. */
function linkState(): SpanExplorerState | null {
  if (rawMode) return null;
  const v = view();
  return {
    data_dir: scope.dataDir,
    max_files: maxFiles,
    bucket: scope.source.bucket || null,
    region: scope.source.region || null,
    credentialMode: scope.source.credentials.kind,
    ...(scope.source.credentials.kind === "role"
      ? { roleArn: scope.source.credentials.roleArn }
      : {}),
    prefix: scope.prefix,
    service: scope.service,
    hosts: scope.hosts,
    start_ns: v.startNs,
    end_ns: v.endNs,
    span_type_uid: selectedUid,
    min_span_ns: band.min_ns,
    max_span_ns: band.max_ns,
  };
}

function syncUrl(): void {
  // Keep the pathname explicit: a bare "?qs" would resolve against <base href="/">
  // and rewrite this page's path to "/".
  const next = `${window.location.pathname}?${buildBrowserQuery(
    scope, view(), analysisView, new URLSearchParams(window.location.search),
  )}${window.location.hash}`;
  if (next !== `${window.location.pathname}${window.location.search}${window.location.hash}`) {
    history.pushState(null, "", next);
  }
}

function changeAnalysisView(next: TailViewState, clearBand = false): void {
  analysisView = next;
  if (clearBand) applyBand({ min_ns: null, max_ns: null });
  else {
    syncUrl();
    renderDetailNow();
  }
}

function restoreUrl(): void {
  const next = new URLSearchParams(window.location.search);
  // The load source is fixed for this document. A different source needs a
  // fresh boot, including restoration of its tab-scoped credentials.
  const nextScope = readScope(next, scope.source);
  if (JSON.stringify({ ...nextScope, source: toUrlSourceScope(nextScope.source) }) !==
      JSON.stringify({ ...scope, source: toUrlSourceScope(scope.source) })) {
    stopStreaming();
    stopComparisonStreaming(true);
    tailAnalysis.mount(null, null);
    window.location.reload();
    return;
  }
  const previous = view();
  selectedUid = next.get("span_type_uid");
  analysisView = readTailView(next);
  band = {
    min_ns: next.has("min_span_ns") ? Number(next.get("min_span_ns")) : null,
    max_ns: next.has("max_span_ns") ? Number(next.get("max_span_ns")) : null,
  };
  attrFilters = parseAttrFilterParams(next.getAll("attr"));
  maxFiles = next.has("max_files") ? Number(next.get("max_files")) : null;
  els.fStart.value = next.has("start_ns") ? nsToPickerUtc(next.get("start_ns")) : "";
  els.fEnd.value = next.has("end_ns") ? nsToPickerUtc(next.get("end_ns")) : "";
  const current = view();
  if (previous.startNs !== current.startNs || previous.endNs !== current.endNs ||
      previous.maxFiles !== current.maxFiles) {
    if (!rawMode) startStreaming("replace");
    else renderDetailNow();
    return;
  }
  const changedSpan = previous.selectedUid !== selectedUid;
  const changedSelection = changedSpan ||
    previous.bandMinNs !== band.min_ns || previous.bandMaxNs !== band.max_ns;
  const changedFilters = JSON.stringify(previous.attrFilters) !== JSON.stringify(attrFilters);
  if (changedSelection) {
    if (activeStreamMode === "exemplars" || activeStreamMode === "refine") stopStreaming();
    if (changedSpan) stopComparisonStreaming(true);
    catalogExpanded = false;
  }
  const refreshExamples = changedSelection && selectedUid != null && !rawMode &&
    exemplarScopeByUid.get(selectedUid) !== exemplarScopeKey(current);
  if (refreshExamples) invalidateSelectedExemplarData();
  if (changedSelection) exemplarRefreshPending = refreshExamples;
  renderCatalogNow();
  renderFilterBarNow();
  if (changedSpan || changedFilters) startComparisonStreaming();
  else renderDetailNow();
  if (refreshExamples) startStreaming("exemplars");
}

// ── Rendering ──

function statsBadge(): string {
  const total = spanTypes.reduce((s, t) => s + t.count, 0);
  let badge = `${spanTypes.length} span types · ${total.toLocaleString()} instances`;
  if (lastCoverage) badge += ` · ${formatCoverageBadge(lastCoverage)}`;
  return badge;
}

function renderDetailNow(loading = false): void {
  const selected = loading
    ? undefined
    : spanTypes.find(s => s.span_type_uid === selectedUid);
  renderDetail(els.detailPanel, {
    spanType: selected,
    catalogExpanded,
    spanTypeCount: spanTypes.length,
    onToggleCatalog: toggleCatalog,
    loading,
    band,
    coverage: lastCoverage,
    attrFilters,
    comparison:
      attrFilters.length > 0
        ? {
            spanType: comparisonSpanType,
            coverage: comparisonCoverage,
            pending: comparisonPending,
            hasSnapshot: comparisonHasSnapshot,
            error: comparisonError,
            pair: matchedHistogramPair,
          }
        : null,
    overrides,
    rawMode,
    exemplarRefreshPending,
    exemplarPreviewAvailable,
    linkState: linkState(),
    source: scope.source,
    rawTrace: scope.trace,
    onBand: applyBand,
    onClearBand: () => applyBand({ min_ns: null, max_ns: null }),
    onToggleFilter: toggleAttrFilter,
    onSetOverride: (id, value) => {
      overrides = setOverride(overrides, id, value);
      saveOverrides(overrides);
      renderDetailNow();
    },
  });
  tailAnalysis.mount(els.detailPanel.querySelector(".span-tail-mount"), selected ? {
    spanType: selected, scope, view: view(), analysis: analysisView,
    headers: Dial9Session.headers(Dial9Creds.headers()),
    onClearBand: () => applyBand({ min_ns: null, max_ns: null }),
    onBand: (minMs, maxMs) => applyBand({
      min_ns: minMs == null ? null : Math.round(minMs * 1e6),
      max_ns: maxMs == null ? null : Math.round(maxMs * 1e6),
    }),
    onAnalysisChange: changeAnalysisView,
  } : null);
}

function renderCatalogNow(): void {
  renderCatalog(els, spanTypes, sort, selectedUid, selectSpanType);
  const selected = spanTypes.find(s => s.span_type_uid === selectedUid);
  const expanded = !selected || catalogExpanded;
  els.catalogWrap.style.display = spanTypes.length && expanded ? "block" : "none";
}

function renderFilterBarNow(): void {
  renderFilterBar(
    els.filterBar,
    attrFilters,
    comparisonPending,
    comparisonError,
    toggleAttrFilter,
    () => {
      attrFilters = [];
      applyAttrFilters();
    },
  );
}

function showError(message: string): void {
  els.loading.classList.add("hidden");
  els.error.style.display = "flex";
  els.error.textContent = message;
}

// ── Selection and band ──

/**
 * Duration-scoped exemplar fields belong to the bounds of the request that
 * produced them. Clear the selected type's copy BEFORE changing bounds, so a
 * previous band's count or rows are never relabeled as the new scope's.
 */
function invalidateSelectedExemplarData(): void {
  exemplarPreviewAvailable = false;
  spanTypes = spanTypes.map((st) => {
    if (st.span_type_uid !== selectedUid) return st;
    const next = { ...st, exemplars: [] };
    delete next.selected_duration_count;
    return next;
  });
}

function selectSpanType(uid: string): void {
  // A pending preserve-mode response is scoped to the PRIOR type and bounds.
  // Cancel it even when the newly selected type already has valid cached rows.
  if (activeStreamMode === "exemplars" || activeStreamMode === "refine")
    stopStreaming();
  stopComparisonStreaming(true);
  if (uid !== selectedUid) {
    analysisView = { ...analysisView, groupKey: null, groupValue: null, property: null };
  }
  selectedUid = uid;
  catalogExpanded = false;
  band = { min_ns: null, max_ns: null };
  const needsRefresh =
    !rawMode && exemplarScopeByUid.get(uid) !== exemplarScopeKey(view());
  if (needsRefresh) invalidateSelectedExemplarData();
  exemplarRefreshPending = needsRefresh;
  syncUrl();
  renderCatalogNow();
  renderDetailNow();
  els.detailPanel.querySelector<HTMLButtonElement>(".change-span")?.focus({ preventScroll: true });
  if (needsRefresh) startStreaming("exemplars");
  if (attrFilters.length > 0) startComparisonStreaming();
}

/**
 * Adopt a new duration band and refetch bounded exemplar candidates. Client-side
 * filtering alone cannot recover lower-band spans from the backend's global
 * top-N list - the backend re-selects exemplars WITHIN the band.
 */
function applyBand(next: DurationBand): void {
  band = next;
  if (!rawMode) invalidateSelectedExemplarData();
  exemplarRefreshPending = !rawMode;
  syncUrl();
  renderDetailNow();
  if (!rawMode) startStreaming("exemplars");
}

// ── Attribute filters ──

function applyAttrFilters(): void {
  syncUrl();
  comparisonRefreshAfterBaseline = false;
  startComparisonStreaming();
}

function toggleAttrFilter(key: string, value: string): void {
  if (rawMode) return; // no backend to re-query
  attrFilters = hasAttrFilter(attrFilters, key, value)
    ? removeAttrFilter(attrFilters, key, value)
    : addAttrFilter(attrFilters, key, value);
  applyAttrFilters();
}

// ── Streaming ──

function updateStreamingUi(): void {
  const baselineActive = activeStreamMode != null;
  const anyActive = baselineActive || comparisonStreaming;
  els.btnStop.disabled = !anyActive;
  els.btnStop.style.opacity = anyActive ? "1" : "0.4";
  els.btnMore.disabled = baselineActive;
  els.btnMore.style.opacity = baselineActive ? "0.4" : "1";
}

function stopStreaming(): void {
  streamToken++;
  activeStreamMode = null;
  if (abortCtl) {
    abortCtl.abort();
    abortCtl = null;
  }
  updateStreamingUi();
}

function startStreaming(mode: StreamMode): void {
  const baselineFilesFolded =
    (mode === "refine" || mode === "exemplars") && lastCoverage
      ? lastCoverage.files_folded
      : 0;
  const baselineFoldedSetId =
    mode === "exemplars" && lastCoverage
      ? (lastCoverage.folded_set_id ?? null)
      : null;

  stopStreaming();
  if (mode === "replace") {
    stopComparisonStreaming(true);
    comparisonRefreshAfterBaseline = false;
    renderFilterBarNow();
  }
  activeStreamMode = mode;
  updateStreamingUi();
  els.error.style.display = "none";
  if (mode === "replace") {
    catalogExpanded = false;
    spanTypes = [];
    lastCoverage = null;
    exemplarScopeByUid.clear();
    els.loading.classList.remove("hidden");
    els.stats.textContent = "Loading span statistics…";
    els.catalogWrap.style.display = "none";
    // The new time scope has no authoritative detail yet. Retire its old
    // analysis request and remove stale graph actions until a snapshot arrives.
    renderDetailNow(true);
  }

  const token = ++streamToken;
  abortCtl = new AbortController();
  const requestUid = selectedUid;
  const requestScopeKey = exemplarScopeKey(view());
  let exemplarSnapshotAdopted = false;
  let gotEvent = false;
  if (mode === "exemplars") exemplarPreviewAvailable = false;

  /** Is this callback still for the selection that requested it? */
  const stillCurrent = (): boolean =>
    mode !== "exemplars" ||
    exemplarRequestMatches(
      requestUid,
      requestScopeKey,
      selectedUid,
      exemplarScopeKey(view()),
    );

  void openSse(buildApiUrl(mode, scope, view(), window.location.origin), {
    headers: Dial9Session.headers(Dial9Creds.headers()),
    signal: abortCtl.signal,
    onEvent: (obj) => {
      if (token !== streamToken || !stillCurrent()) return;
      const resp = obj as SpanStatsResponse;
      gotEvent = true;
      els.loading.classList.add("hidden");
      const incomingTypes = resp.span_types ?? [];
      const incomingFilesFolded = resp.coverage?.files_folded ?? 0;

      if (mode === "exemplars") {
        exemplarSnapshotAdopted = false;
        const membership = classifyExemplarSnapshot(
          baselineFoldedSetId,
          resp.coverage?.folded_set_id ?? null,
          resp.coverage?.target_folded_set_id ?? null,
        );
        if (membership.preview) {
          const merged = mergeSelectedExemplarSnapshot(
            spanTypes,
            incomingTypes,
            requestUid,
          );
          spanTypes = merged.spanTypes;
          exemplarPreviewAvailable = merged.matched;
          exemplarSnapshotAdopted = membership.complete && merged.matched;
          renderDetailNow();
        }
      } else if (
        shouldAdoptCatalogSnapshot(
          mode,
          baselineFilesFolded,
          incomingFilesFolded,
        )
      ) {
        spanTypes = incomingTypes;
        exemplarScopeByUid.clear();
        for (const st of incomingTypes) {
          exemplarScopeByUid.set(st.span_type_uid, requestScopeKey);
        }
        lastCoverage = resp.coverage ?? null;
        exemplarRefreshPending = false;
        captureMatchedHistogramPair();
        renderCatalogNow();
        renderDetailNow();
        // A comparison restored from the URL can begin as soon as its baseline
        // span type exists. Its separate request leaves this stream untouched.
        if (
          attrFilters.length > 0 &&
          selectedUid != null &&
          !comparisonStreaming &&
          !comparisonHasSnapshot &&
          comparisonError == null
        ) {
          startComparisonStreaming();
        }
      }
      els.stats.innerHTML = "";
      els.stats.append(
        statsBadge() + " · ",
        Object.assign(document.createElement("span"), {
          className: "spinner",
          style: "width:0.9em;height:0.9em;vertical-align:middle",
        }),
        " refining…",
      );
    },
    onClose: () => {
      if (token !== streamToken) return;
      activeStreamMode = null;
      updateStreamingUi();
      if (!stillCurrent()) return;
      if (mode === "exemplars") {
        const completed = completeExemplarRefresh(
          spanTypes,
          lastCoverage,
          exemplarSnapshotAdopted,
        );
        spanTypes = completed.spanTypes;
        lastCoverage = completed.coverage;
        exemplarRefreshPending = completed.pending;
        if (exemplarSnapshotAdopted && requestUid != null) {
          exemplarScopeByUid.set(requestUid, requestScopeKey);
        }
        renderDetailNow();
      } else {
        exemplarRefreshPending = false;
      }
      if (gotEvent) {
        els.stats.textContent =
          statsBadge() +
          (mode === "exemplars" && !exemplarSnapshotAdopted
            ? " · exemplar refresh incomplete"
            : " · refined");
      } else if (mode === "replace") {
        showError("No span statistics returned for this source.");
      }
      if (mode === "replace" || mode === "refine") {
        refreshComparisonAfterBaseline();
      }
    },
    onError: (err) => {
      if (token !== streamToken) return;
      activeStreamMode = null;
      updateStreamingUi();
      if (!stillCurrent()) return;
      exemplarRefreshPending = mode === "exemplars";
      if (mode === "exemplars") {
        renderDetailNow();
        els.stats.textContent = `${statsBadge()} · exemplar refresh interrupted`;
      } else if (mode === "refine") {
        els.stats.textContent = `${statsBadge()} · refinement interrupted`;
      } else if (!gotEvent) {
        showError(`Failed to load span stats: ${err.message}`);
      } else {
        els.stats.textContent = `${statsBadge()} · interrupted`;
      }
      if (mode === "replace" || mode === "refine") {
        refreshComparisonAfterBaseline();
      }
    },
  });
}

// ── Histogram comparison stream ──

function comparisonRequestKey(): string | null {
  if (rawMode || selectedUid == null || attrFilters.length === 0) return null;
  return buildApiUrl("comparison", scope, view(), window.location.origin);
}

function stopComparisonStreaming(clearData: boolean): void {
  comparisonStreamToken++;
  comparisonStreaming = false;
  activeComparisonKey = null;
  comparisonPending = false;
  if (comparisonAbortCtl) {
    comparisonAbortCtl.abort();
    comparisonAbortCtl = null;
  }
  if (clearData) {
    comparisonSpanType = null;
    comparisonCoverage = null;
    comparisonHasSnapshot = false;
    comparisonError = null;
    matchedHistogramPair = null;
  }
  updateStreamingUi();
}

function captureMatchedHistogramPair(): void {
  matchedHistogramPair = retainMatchedHistogramPair(
    matchedHistogramPair,
    spanTypes.find((spanType) => spanType.span_type_uid === selectedUid),
    lastCoverage,
    comparisonSpanType,
    comparisonCoverage,
    comparisonHasSnapshot,
  );
}

function startComparisonStreaming(preserveData = false): void {
  const requestKey = comparisonRequestKey();
  if (requestKey == null) {
    stopComparisonStreaming(true);
    renderFilterBarNow();
    renderDetailNow();
    return;
  }

  stopComparisonStreaming(!preserveData);
  comparisonPending = true;
  comparisonError = null;
  comparisonStreaming = true;
  activeComparisonKey = requestKey;
  updateStreamingUi();
  renderFilterBarNow();
  renderDetailNow();

  const token = ++comparisonStreamToken;
  const requestUid = selectedUid;
  let gotEvent = false;
  comparisonAbortCtl = new AbortController();

  const stillCurrent = (): boolean =>
    token === comparisonStreamToken &&
    activeComparisonKey === requestKey &&
    requestKey === comparisonRequestKey();

  void openSse(requestKey, {
    headers: Dial9Session.headers(Dial9Creds.headers()),
    signal: comparisonAbortCtl.signal,
    onEvent: (obj) => {
      if (!stillCurrent()) return;
      const resp = obj as SpanStatsResponse;
      gotEvent = true;
      comparisonSpanType =
        resp.span_types?.find(
          (spanType) => spanType.span_type_uid === requestUid,
        ) ?? null;
      comparisonCoverage = resp.coverage ?? null;
      comparisonHasSnapshot = true;
      captureMatchedHistogramPair();
      renderFilterBarNow();
      renderDetailNow();
    },
    onClose: () => {
      if (!stillCurrent()) return;
      comparisonStreaming = false;
      activeComparisonKey = null;
      comparisonPending = false;
      comparisonAbortCtl = null;
      if (!gotEvent) comparisonError = "No comparison data returned.";
      updateStreamingUi();
      renderFilterBarNow();
      renderDetailNow();

      if (comparisonRefreshAfterBaseline) {
        comparisonRefreshAfterBaseline = false;
        const baselineSet = lastCoverage?.folded_set_id ?? null;
        const comparisonSet = comparisonCoverage?.folded_set_id ?? null;
        if (baselineSet != null && baselineSet !== comparisonSet) {
          startComparisonStreaming(true);
        }
      }
    },
    onError: (err) => {
      if (!stillCurrent()) return;
      comparisonStreaming = false;
      activeComparisonKey = null;
      comparisonPending = false;
      comparisonAbortCtl = null;
      comparisonError = `Comparison failed: ${err.message}`;
      updateStreamingUi();
      renderFilterBarNow();
      renderDetailNow();
    },
  });
}

/**
 * A baseline stream may add folded files while a comparison is reading the old
 * set. Refresh once at baseline completion so both rows cover the same files.
 */
function refreshComparisonAfterBaseline(): void {
  if (comparisonRequestKey() == null) return;
  const baselineSet = lastCoverage?.folded_set_id ?? null;
  const comparisonSet = comparisonCoverage?.folded_set_id ?? null;
  if (
    baselineSet == null ||
    (comparisonHasSnapshot && baselineSet === comparisonSet)
  )
    return;
  if (comparisonStreaming) {
    comparisonRefreshAfterBaseline = true;
  } else {
    startComparisonStreaming(comparisonHasSnapshot);
  }
}

// ── Event handlers ──

window.addEventListener("popstate", restoreUrl);

els.btnApply.addEventListener("click", () => {
  maxFiles = null;
  syncUrl();
  startStreaming("replace");
});

els.btnMore.addEventListener("click", () => {
  // Grow from the BOUNDED WORK the server was allowed, not from how many cached
  // files the snapshot covers - an all-cache scope would otherwise jump straight
  // to the ceiling in one click.
  maxFiles = nextMaxFiles(refinementWorkDepth(lastCoverage, maxFiles));
  syncUrl();
  startStreaming("refine");
});

els.btnStop.addEventListener("click", () => {
  const stoppedMode = activeStreamMode;
  const stoppedComparison = comparisonStreaming;
  stopStreaming();
  stopComparisonStreaming(false);
  if (stoppedComparison) comparisonError = "Comparison stopped.";
  if (stoppedMode !== "exemplars") exemplarRefreshPending = false;
  if (stoppedMode === "replace" && spanTypes.length === 0) {
    showError("Span statistics loading cancelled.");
  }
  renderFilterBarNow();
  renderDetailNow();
  if (stoppedMode != null) els.stats.textContent = `${statsBadge()} · stopped`;
});

els.btnCopyLink.addEventListener("click", async () => {
  const url = window.location.href;
  const orig = els.btnCopyLink.textContent;
  try {
    await navigator.clipboard.writeText(url);
  } catch {
    window.prompt("Copy this link:", url);
  }
  els.btnCopyLink.textContent = "Copied!";
  setTimeout(() => {
    els.btnCopyLink.textContent = orig;
  }, 1500);
});

els.catalog.querySelector("thead")?.addEventListener("click", (e) => {
  const th = (e.target as HTMLElement).closest<HTMLElement>("th[data-sort]");
  const key = th?.dataset["sort"];
  if (!key) return;
  sort = nextSort(sort, key as SortKey);
  renderCatalogNow();
});

function toggleCatalog(): void {
  catalogExpanded = !catalogExpanded;
  renderCatalogNow();
  renderDetailNow();
  if (catalogExpanded) {
    els.catalogBody.querySelector(".selected")?.scrollIntoView({ block: "nearest" });
  }
}

// ── Boot ──

renderFilterBarNow();

if (rawMode && scope.trace != null) {
  // Raw mode drives no stream, so the SSE-only toolbar controls are meaningless.
  els.toolbar.style.display = "none";
  els.stats.textContent = "📂 Raw trace mode — loading…";
  void (async () => {
    try {
      const traceBytes = await fetchRawTraceBytes(
        scope.trace as string,
        window.location.origin,
        Dial9Session.headers(Dial9Creds.headers()),
      );
      const response = await requestRawSpanStats(
        traceBytes,
        window.location.origin,
      );
      spanTypes = response.span_types;
      els.loading.classList.add("hidden");
      els.stats.textContent = `📂 Server-decoded raw trace · ${rawStatsSummary(response)}`;
      renderCatalogNow();
      renderDetailNow();
    } catch (e) {
      showError(
        `Failed to load raw trace: ${e instanceof Error ? e.message : String(e)}`,
      );
    }
  })();
} else if (aggregate) {
  startStreaming("replace");
} else {
  showError(
    "No scope provided. Open the Span Explorer from the trace browser, " +
      "with ?api=1&bucket=…, or with ?trace=<url> for a local trace file.",
  );
}
