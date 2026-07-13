// The Tokio Stats page entry. Entirely an aggregated server-driven surface (no
// trace bytes decoded): the primary consumer of /api/tokio-stats via the typed
// aggregates client (fetchTokioStats). The refine loop is the page's own
// (shouldStopRefining), driving fetchTokioStats per poll with 300ms pacing
// rather than the generic refineUntilFrozen helper.

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

/** One comparison period (module-scoped state). */
interface Period {
  id: number;
  startNs: string | null;
  endNs: string | null;
  data: TokioStatsResponse | null;
}

// Render the "Switch to legacy UI" control. The ui-switch.js <head> auto-boot
// is a no-op on this off-root path.
if (window.D9UiSwitch) {
  window.D9UiSwitch.mount({ side: "new" });
} else {
  console.warn("ui-switch.js is not loaded; the UI switch control is unavailable");
}

// URL params are read ONCE: the scope is fixed for the page's lifetime, and
// auto-load checks the ORIGINAL query before syncUrl rewrites it.
const originalParams = new URLSearchParams(window.location.search);
const scope = readScope(originalParams);

const els = pageEls();

let periods: Period[] = [];
let periodIdCounter = 0;
let activeTab = "diff"; // "diff" or "p0", "p1", ...

// ── Period management ──

function renderPeriodsNow(): void {
  renderPeriods(els.periods, periods, els.utcToggle.checked, {
    onUpdate: updatePeriod,
    onRemove: removePeriod,
  });
}

function syncUrl(): void {
  // Keep the pathname explicit: a bare "?qs" would resolve against this page's
  // <base href="/"> and rewrite the off-root path to "/".
  history.replaceState(null, "", window.location.pathname + "?" + buildSyncQuery(scope, periods));
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

// ── Threshold ──

function updateThreshLabel(): void {
  els.threshLabel.textContent = formatDuration(thresholdNs(els.slider.value));
}

// ── Exemplar deep links ──

function openExemplar(url: string): void {
  window.open(url, "_blank");
}

// ── Render pipeline ──

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

  // A specific period's detail tab -> single-period view.
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

  // Diff view.
  const model = buildDiffModel(stats, periods.length);
  renderDiff(els.summary, els.tableContainer, model, periods.length);
}

// ── Data loading + refinement loop ──

async function loadPeriod(period: Period): Promise<void> {
  let refine = false;
  let prevFolded = -1;
  for (;;) {
    // Build the scope query, setting each param only when present / truthy.
    // Omitted keys keep the server's serde defaults; explicit `undefined` is
    // rejected under exactOptionalPropertyTypes.
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
      // 404 (no agg context / no source files match this scope): throw the
      // response body text and paint the status line red. "disabled" never
      // occurs here: aggregationEnabled is not passed, so the client issues the
      // request unconditionally.
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
    // All periods load concurrently, each with its own refine loop.
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
    // of the same span.
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

// ── Init: restore periods from the URL, render, sync once ──

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

// Unified keys: `?` help overlay (the page had no keyboard).
mountTokioStatsKeys();

// Auto-load on open when the original URL carried start_ns or bucket.
if (shouldAutoLoad(originalParams)) void loadAll();
