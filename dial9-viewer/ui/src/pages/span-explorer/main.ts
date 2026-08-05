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
  mergeSelectedExemplarSnapshot,
  nextMaxFiles,
  nsToPickerUtc,
  openSse,
  parseAttrFilterParams,
  pickerUtcToNs,
  refinementWorkDepth,
  removeAttrFilter,
  shouldAdoptCatalogSnapshot,
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
import { pageEls } from "./dom.js";
import { renderCatalog, nextSort, type CatalogSort, type SortKey } from "./catalog.js";
import { renderDetail } from "./detail.js";
import { renderFilterBar } from "./filters.js";
import {
  buildApiUrl,
  buildBrowserQuery,
  exemplarScopeKey,
  isAggregateMode,
  readScope,
  sourceScope,
  type ViewState,
} from "./scope.js";
import {
  loadOverrides,
  saveOverrides,
  setOverride,
  type ColumnOverrides,
} from "./columns.js";
import { fetchRawTraceBytes, rawStatsSummary, requestRawSpanStats } from "./raw.js";

const els = pageEls();

// URL params are read ONCE: the load scope is fixed for the page's lifetime.
const params = new URLSearchParams(window.location.search);
const scope = readScope(params);
const aggregate = isAggregateMode(params, scope);
const rawMode = scope.trace != null;

// Restore the link's reader-role (and region) into the creds store so this tab
// has an identity to read the bucket with. The role then rides as a HEADER on
// every /api/span-stats request; buildApiUrl deliberately omits aws_role_arn
// from the request URL (a role on both header and query is the server's
// ConflictingCredentials 400). Region rides both transports (see SourceScope).
applyToCreds(sourceScope(scope), Dial9Creds);

// ── Mutable page state ──

let spanTypes: SpanTypeStats[] = [];
let selectedUid = params.get("span_type_uid");
let band: DurationBand = {
  min_ns: params.get("min_span_ns") != null ? Number(params.get("min_span_ns")) : null,
  max_ns: params.get("max_span_ns") != null ? Number(params.get("max_span_ns")) : null,
};
let attrFilters: readonly AttrFilter[] = parseAttrFilterParams(params.getAll("attr"));
let maxFiles: number | null =
  params.get("max_files") != null ? Number(params.get("max_files")) : null;
let lastCoverage: Coverage | null = null;
let sort: CatalogSort = { key: "count", ascending: false };
let overrides: ColumnOverrides = loadOverrides();

// Which duration scope each type's cached exemplars were produced under, so a
// reselect can tell a stale set from a valid one.
const exemplarScopeByUid = new Map<string, string>();
let exemplarRefreshPending = false;
let exemplarPreviewAvailable = false;

// Stream state. A monotonic token retires callbacks from a superseded stream.
let streamToken = 0;
let abortCtl: AbortController | null = null;
let activeStreamMode: StreamMode | null = null;

if (params.get("start_ns")) els.fStart.value = nsToPickerUtc(params.get("start_ns"));
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
    bucket: scope.bucket,
    region: scope.region,
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
  history.replaceState(
    null,
    "",
    `${window.location.pathname}?${buildBrowserQuery(scope, view())}`,
  );
}

// ── Rendering ──

function statsBadge(): string {
  const total = spanTypes.reduce((s, t) => s + t.count, 0);
  let badge = `${spanTypes.length} span types · ${total.toLocaleString()} instances`;
  if (lastCoverage) badge += ` · ${formatCoverageBadge(lastCoverage)}`;
  return badge;
}

function renderDetailNow(): void {
  renderDetail(els.detailPanel, {
    spanType: spanTypes.find((s) => s.span_type_uid === selectedUid),
    band,
    coverage: lastCoverage,
    attrFilters,
    overrides,
    rawMode,
    exemplarRefreshPending,
    exemplarPreviewAvailable,
    linkState: linkState(),
    rawTrace: scope.trace,
    rawRegion: scope.region,
    onBand: applyBand,
    onClearBand: () => applyBand({ min_ns: null, max_ns: null }),
    onToggleFilter: toggleAttrFilter,
    onSetOverride: (id, value) => {
      overrides = setOverride(overrides, id, value);
      saveOverrides(overrides);
      renderDetailNow();
    },
    onResetOverrides: () => {
      overrides = {};
      saveOverrides(overrides);
      renderDetailNow();
    },
  });
}

function renderCatalogNow(): void {
  renderCatalog(els, spanTypes, sort, selectedUid, selectSpanType);
}

function renderFilterBarNow(): void {
  renderFilterBar(els.filterBar, attrFilters, toggleAttrFilter, () => {
    attrFilters = [];
    applyAttrFilters();
  });
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
  if (activeStreamMode === "exemplars" || activeStreamMode === "refine") stopStreaming();
  selectedUid = uid;
  band = { min_ns: null, max_ns: null };
  const needsRefresh = !rawMode && exemplarScopeByUid.get(uid) !== exemplarScopeKey(view());
  if (needsRefresh) invalidateSelectedExemplarData();
  exemplarRefreshPending = needsRefresh;
  syncUrl();
  renderCatalogNow();
  renderDetailNow();
  if (needsRefresh) startStreaming("exemplars");
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
  renderFilterBarNow();
  if (!rawMode) startStreaming("replace");
}

function toggleAttrFilter(key: string, value: string): void {
  if (rawMode) return; // no backend to re-query
  attrFilters = hasAttrFilter(attrFilters, key, value)
    ? removeAttrFilter(attrFilters, key, value)
    : addAttrFilter(attrFilters, key, value);
  applyAttrFilters();
}

// ── Streaming ──

function setStreamingUi(active: boolean): void {
  els.btnStop.disabled = !active;
  els.btnStop.style.opacity = active ? "1" : "0.4";
  els.btnMore.disabled = active;
  els.btnMore.style.opacity = active ? "0.4" : "1";
}

function stopStreaming(): void {
  streamToken++;
  activeStreamMode = null;
  if (abortCtl) {
    abortCtl.abort();
    abortCtl = null;
  }
  setStreamingUi(false);
}

function startStreaming(mode: StreamMode): void {
  const baselineFilesFolded =
    (mode === "refine" || mode === "exemplars") && lastCoverage ? lastCoverage.files_folded : 0;
  const baselineFoldedSetId =
    mode === "exemplars" && lastCoverage ? (lastCoverage.folded_set_id ?? null) : null;

  stopStreaming();
  activeStreamMode = mode;
  setStreamingUi(true);
  els.error.style.display = "none";
  if (mode === "replace") {
    els.loading.classList.remove("hidden");
    els.catalogWrap.style.display = "none";
    els.detailPanel.className = "detail-panel empty";
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
    exemplarRequestMatches(requestUid, requestScopeKey, selectedUid, exemplarScopeKey(view()));

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
          const merged = mergeSelectedExemplarSnapshot(spanTypes, incomingTypes, requestUid);
          spanTypes = merged.spanTypes;
          exemplarPreviewAvailable = merged.matched;
          exemplarSnapshotAdopted = membership.complete && merged.matched;
          renderDetailNow();
        }
      } else if (shouldAdoptCatalogSnapshot(mode, baselineFilesFolded, incomingFilesFolded)) {
        spanTypes = incomingTypes;
        exemplarScopeByUid.clear();
        for (const st of incomingTypes) {
          exemplarScopeByUid.set(st.span_type_uid, requestScopeKey);
        }
        lastCoverage = resp.coverage ?? null;
        exemplarRefreshPending = false;
        renderCatalogNow();
        renderDetailNow();
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
      setStreamingUi(false);
      activeStreamMode = null;
      if (!stillCurrent()) return;
      if (mode === "exemplars") {
        const completed = completeExemplarRefresh(spanTypes, lastCoverage, exemplarSnapshotAdopted);
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
      }
    },
    onError: (err) => {
      if (token !== streamToken) return;
      setStreamingUi(false);
      activeStreamMode = null;
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
    },
  });
}

// ── Event handlers ──

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
  stopStreaming();
  if (stoppedMode !== "exemplars") exemplarRefreshPending = false;
  renderDetailNow();
  els.stats.textContent = `${statsBadge()} · stopped`;
});

els.btnCopyLink.addEventListener("click", async () => {
  const url = window.location.origin + window.location.pathname + window.location.search;
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
      const response = await requestRawSpanStats(traceBytes, window.location.origin);
      spanTypes = response.span_types;
      els.loading.classList.add("hidden");
      els.stats.textContent = `📂 Server-decoded raw trace · ${rawStatsSummary(response)}`;
      renderCatalogNow();
      renderDetailNow();
    } catch (e) {
      showError(`Failed to load raw trace: ${e instanceof Error ? e.message : String(e)}`);
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
