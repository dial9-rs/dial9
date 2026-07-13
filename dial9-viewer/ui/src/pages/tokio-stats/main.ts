// src/pages/tokio-stats/main.ts - the migrated Tokio Stats page entry (T41;
// Vite entry new/tokio_stats.html; ADR-0004 section 8). A behavior-
// preserving typed port of the legacy inline IIFE (tokio_stats.html:65-429;
// features/04 is the contract). Same treatment as T13/T14: declarative
// rendering (lit-html, N17 - the XSS structural guard), the frozen-core /
// server boundary through lib/trace, the dual-UI switch injected, defects
// carried not fixed (H4 dead exemplar link, D4 no coverage UI, G3 diff
// crash, E2 invisible mixed/unknown - all preserved).
//
// The page is ENTIRELY an aggregated server-driven surface (no trace bytes
// are decoded): it is the PRIMARY consumer of /api/tokio-stats, driven
// through T18's typed aggregates client (fetchTokioStats). The refine LOOP
// is the page's own (shouldStopRefining) - it must match the legacy
// termination + 300ms pacing exactly for the behavioral differ, so it drives
// fetchTokioStats per poll rather than the generic refineUntilFrozen helper
// (which does one extra poll in the fully-folded case and has no pacing).

import { Dial9Creds, fetchTokioStats } from "../../lib/trace/index.js";
import type { TokioStatsQuery, TokioStatsResponse } from "../../lib/trace/index.js";
import { pageEls } from "./dom.js";
import { formatDuration, datetimeToNs, thresholdNs } from "./format.js";
import {
  computeStats,
  buildDiffModel,
  shouldStopRefining,
} from "./stats.js";
import {
  renderPeriods,
  renderTabs,
  renderSinglePeriod,
  renderNotLoaded,
  renderDiff,
  type Tab,
} from "./render.js";
import {
  readScope,
  parseInitialPeriods,
  buildSyncQuery,
  shouldAutoLoad,
} from "./url.js";
import { mountTokioStatsKeys } from "./keys.js";

/** One comparison period (module-scoped state, exactly as the legacy IIFE). */
interface Period {
  id: number;
  startNs: string | null;
  endNs: string | null;
  data: TokioStatsResponse | null;
}

// Dual-UI switch (T38): render the "Switch to legacy UI" control. The
// ui-switch.js <head> auto-boot is a no-op on this off-root path.
if (window.D9UiSwitch) {
  window.D9UiSwitch.mount({ side: "new" });
} else {
  console.warn("ui-switch.js is not loaded; the UI switch control is unavailable");
}

// URL params are read ONCE (A3): the scope is fixed for the page's lifetime,
// and auto-load (A6) checks the ORIGINAL query before syncUrl rewrites it.
const originalParams = new URLSearchParams(window.location.search);
const scope = readScope(originalParams);

const els = pageEls();

let periods: Period[] = [];
let periodIdCounter = 0;
let activeTab = "diff"; // "diff" or "p0", "p1", ...

// ── Period management (section B) ──

function renderPeriodsNow(): void {
  renderPeriods(els.periods, periods, els.utcToggle.checked, {
    onUpdate: updatePeriod,
    onRemove: removePeriod,
  });
}

function syncUrl(): void {
  history.replaceState(null, "", "?" + buildSyncQuery(scope, periods));
}

function addPeriod(startNs: string | null, endNs: string | null): void {
  periods.push({
    id: periodIdCounter++,
    startNs: startNs || null,
    endNs: endNs || null,
    data: null,
  });
  renderPeriodsNow();
  syncUrl();
}

function removePeriod(id: number): void {
  periods = periods.filter((p) => p.id !== id);
  renderPeriodsNow();
  syncUrl();
  renderFromCache();
}

function updatePeriod(id: number, field: "start" | "end", val: string): void {
  const p = periods.find((x) => x.id === id);
  if (p) {
    const ns = datetimeToNs(val, els.utcToggle.checked);
    if (field === "start") p.startNs = ns;
    else p.endNs = ns;
  }
  syncUrl();
}

// ── Threshold (section C) ──

function updateThreshLabel(): void {
  els.threshLabel.textContent = formatDuration(thresholdNs(els.slider.value));
}

// ── Exemplar deep links (section H) ──

function openExemplar(url: string): void {
  window.open(url, "_blank");
}

// ── Render pipeline (sections E/F/G) ──

function renderTabsNow(stats: (ReturnType<typeof computeStats>)[]): void {
  const loaded = stats.filter((s) => s).length;
  if (loaded <= 1) {
    els.viewTabs.style.display = "none";
    return;
  }
  els.viewTabs.style.display = "";
  const tabs: Tab[] = [{ id: "diff", label: "⚡ Diff" }];
  periods.forEach((p, i) => {
    if (stats[i]) tabs.push({ id: `p${i}`, label: `P${i + 1} Detail` });
  });
  renderTabs(els.viewTabs, tabs, activeTab, setTab);
}

function setTab(tab: string): void {
  activeTab = tab;
  renderFromCache();
}

function renderFromCache(): void {
  const threshNs = thresholdNs(els.slider.value);
  const stats = periods.map((p) => computeStats(p.data, threshNs));
  if (stats.every((s) => !s)) return;

  els.summary.style.display = "";
  renderTabsNow(stats);

  const multi = periods.length > 1 && stats.filter((s) => s).length > 1;

  // A specific period's detail tab -> single-period view (G2).
  if (multi && activeTab.startsWith("p")) {
    const idx = parseInt(activeTab.slice(1), 10);
    const s = stats[idx];
    const data = periods[idx]?.data;
    if (!s || !data) {
      renderNotLoaded(els.tableContainer);
      return;
    }
    renderSinglePeriod(
      els.summary,
      els.tableContainer,
      s,
      data,
      threshNs,
      scope.bucket,
      openExemplar,
    );
    return;
  }

  if (!multi) {
    const s = stats.find((x) => x);
    const withData = periods.find((p) => p.data);
    if (!s || !withData || !withData.data) return;
    renderSinglePeriod(
      els.summary,
      els.tableContainer,
      s,
      withData.data,
      threshNs,
      scope.bucket,
      openExemplar,
    );
    return;
  }

  // Diff view (G3-G9).
  const model = buildDiffModel(stats, periods.length);
  renderDiff(els.summary, els.tableContainer, model, periods.length);
}

// ── Data loading + refinement loop (section D) ──

async function loadPeriod(period: Period): Promise<void> {
  let refine = false;
  let prevFolded = -1;
  for (;;) {
    // Build the scope query with only the params the legacy page sets (each
    // is set only when present / truthy - tokio_stats.html:376-384). Omitted
    // keys keep the server's serde defaults; explicit `undefined` is rejected
    // under exactOptionalPropertyTypes.
    const query: TokioStatsQuery = { host: scope.host, refine };
    if (scope.bucket) query.bucket = scope.bucket;
    if (scope.prefix) query.prefix = scope.prefix;
    if (scope.service) query.service = scope.service;
    if (period.startNs) query.start_ns = period.startNs;
    if (period.endNs) query.end_ns = period.endNs;
    const result = await fetchTokioStats(query, {
      headers: Dial9Creds.headers(),
    });
    if (result.kind === "unavailable") {
      // 404 (no agg context / no source files match this scope) - the legacy
      // page throws the response body text and paints the status line red
      // (C2/D5). "disabled" never occurs here: aggregationEnabled is NOT
      // passed, so the client issues the request unconditionally, matching
      // the legacy page which always fetches.
      throw new Error(result.message ?? "");
    }
    const data = result.response;
    period.data = data;
    renderFromCache();
    const cov = data.coverage;
    if (shouldStopRefining(cov, prevFolded)) break;
    prevFolded = cov!.files_folded;
    refine = true;
    await new Promise((r) => setTimeout(r, 300));
  }
}

async function loadAll(): Promise<void> {
  els.status.textContent = "Loading…";
  els.status.className = "status";
  try {
    // All periods load concurrently, each with its own refine loop (D6).
    await Promise.all(periods.map((p) => loadPeriod(p)));
    els.status.textContent = "Complete";
  } catch (e) {
    els.status.textContent =
      "Error: " + (e instanceof Error ? e.message : String(e));
    els.status.className = "status error";
  }
}

// ── Wiring ──

els.utcToggle.addEventListener("change", renderPeriodsNow);
els.btnLoad.addEventListener("click", () => void loadAll());
els.btnAdd.addEventListener("click", () => {
  const last = periods[periods.length - 1];
  if (last && last.startNs && last.endNs) {
    // "Compare against the previous window": the immediately-preceding window
    // of the same span (B4).
    const span = Number(last.endNs) - Number(last.startNs);
    addPeriod(String(Number(last.startNs) - span), last.startNs);
  } else {
    addPeriod(null, null);
  }
});
els.slider.addEventListener("input", () => {
  updateThreshLabel();
  renderFromCache();
});
updateThreshLabel();

// ── Init: restore periods from the URL (A4/A5), render, sync once ──

for (const b of parseInitialPeriods(originalParams)) {
  periods.push({
    id: periodIdCounter++,
    startNs: b.startNs,
    endNs: b.endNs,
    data: null,
  });
}
renderPeriodsNow();
syncUrl();

// Unified keyboard model (T20): `?` help overlay (the page had no keyboard).
mountTokioStatsKeys();

// Auto-load on open when the original URL carried start_ns or bucket (A6).
if (shouldAutoLoad(originalParams)) void loadAll();
