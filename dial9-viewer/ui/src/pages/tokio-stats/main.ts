// The Tokio Stats page entry. Entirely an aggregated server-driven surface (no
// trace bytes decoded): /api/tokio-stats is a Server-Sent Events stream that
// pushes a fresh full snapshot as each source file folds and closes at the cap.
// The server owns the refine/stop loop; the page just renders each snapshot.

import {
  Dial9Creds,
  Dial9Session,
  nextMaxFiles,
  openSse,
  tokioStatsUrl,
} from "../../lib/trace/index.js";
import type { TokioStatsQuery, TokioStatsResponse } from "../../lib/trace/index.js";
import { pageEls } from "./dom.js";
import { formatDuration, datetimeToNs, thresholdNs } from "./format.js";
import { computeStats, buildDiffModel } from "./stats.js";
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
  scopeFromParams,
  parseInitialPeriods,
  buildSyncQuery,
  shouldAutoLoad,
  type ScopeParams,
} from "./url.js";
import { mountTokioStatsKeys } from "./keys.js";
import { parseDiff } from "../../lib/canvas/index.js";

/** One comparison period (module-scoped state). */
interface Period {
  id: number;
  startNs: string | null;
  endNs: string | null;
  data: TokioStatsResponse | null;
  /** In diff mode each side carries its own scope; else undefined (page scope). */
  scope?: ScopeParams;
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
// Diff mode (?diff=1&a=<b64>&b=<b64>): two sides, each its own independent scope.
const diffParsed = parseDiff(originalParams);
const diffMode = diffParsed !== null;

/** The scope a period streams against: its own (diff side) or the page scope. */
function effectiveScope(period: Period): ScopeParams {
  return period.scope ?? scope;
}

const els = pageEls();

let periods: Period[] = [];
let periodIdCounter = 0;
let activeTab = "diff"; // "diff" or "p0", "p1", ...

// The `max_files` fold ceiling, raised by "Load more". null lets the server pick
// its own cap; a fresh Load resets it. Applies to every period's stream.
let maxFiles: number | null = null;

/** Highest files_folded across loaded periods, for the next "Load more" ceiling. */
function maxFolded(): number {
  return Math.max(0, ...periods.map((p) => p.data?.coverage?.files_folded ?? 0));
}

/** "Load more" is offered only while idle and some period can still fold more. */
function refreshMoreBtn(): void {
  els.btnMore.disabled = periods.every((p) => {
    const c = p.data?.coverage;
    return c == null || c.files_folded >= c.files_matched;
  });
}

// ── Period management ──

function renderPeriodsNow(): void {
  renderPeriods(els.periods, periods, els.utcToggle.checked, {
    onUpdate: updatePeriod,
    onRemove: removePeriod,
  });
}

function syncUrl(): void {
  // Diff mode keeps the as-opened `?diff=1&a=&b=` link (each side's independent
  // scope lives in the b64 payload, which buildSyncQuery cannot express); the
  // p{i}_ period sync is for the normal same-scope multi-period case.
  if (diffMode) return;
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

// ── Data loading (SSE stream) ──

async function loadPeriod(period: Period): Promise<void> {
  // Build the scope query, setting each param only when present / truthy.
  // Omitted keys keep the server's serde defaults; explicit `undefined` is
  // rejected under exactOptionalPropertyTypes. No `refine` flag: the stream
  // owns the fold loop.
  const es = effectiveScope(period);
  const query: TokioStatsQuery = { host: es.host };
  if (es.bucket) query.bucket = es.bucket;
  if (es.prefix) query.prefix = es.prefix;
  if (es.service) query.service = es.service;
  if (period.startNs) query.start_ns = period.startNs;
  if (period.endNs) query.end_ns = period.endNs;
  if (maxFiles != null) query.max_files = maxFiles;

  // openSse settles when the stream ends; a fetch/HTTP failure (e.g. 404 when
  // no source files match this scope) arrives via onError, captured here so
  // loadAll's catch paints the status line red.
  let streamErr: Error | null = null;
  await openSse(tokioStatsUrl(query), {
    headers: Dial9Session.headers(Dial9Creds.headers()),
    onEvent: (obj) => {
      period.data = obj as TokioStatsResponse;
      renderFromCache();
    },
    onError: (err) => {
      streamErr = err;
    },
  });
  if (streamErr) throw streamErr;
}

async function loadAll(): Promise<void> {
  els.status.textContent = "Loading…";
  els.status.className = "status";
  els.btnMore.disabled = true;
  try {
    // All periods stream concurrently, each closing at the server's cap.
    await Promise.all(periods.map((p) => loadPeriod(p)));
    els.status.textContent = "Complete";
  } catch (e) {
    els.status.textContent =
      "Error: " + (e instanceof Error ? e.message : String(e));
    els.status.className = "status error";
  } finally {
    refreshMoreBtn();
  }
}

// ── Wiring ──

els.utcToggle.addEventListener("change", renderPeriodsNow);
els.btnLoad.addEventListener("click", () => {
  maxFiles = null; // a fresh load starts from the server's default cap
  void loadAll();
});
els.btnMore.addEventListener("click", () => {
  // Raise the fold ceiling to ~4x what's folded so far and re-stream; the
  // server serves the already-folded files instantly, then folds up to the cap.
  maxFiles = nextMaxFiles(maxFolded());
  void loadAll();
});
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

if (diffParsed) {
  for (const side of [diffParsed.a, diffParsed.b]) {
    periods.push({
      id: periodIdCounter++,
      startNs: side.get("start_ns") || null,
      endNs: side.get("end_ns") || null,
      data: null,
      scope: scopeFromParams(side),
    });
  }
} else {
  for (const b of parseInitialPeriods(originalParams)) {
    periods.push({
      id: periodIdCounter++,
      startNs: b.startNs,
      endNs: b.endNs,
      data: null,
    });
  }
}
renderPeriodsNow();
syncUrl();

// Unified keys: `?` help overlay (the page had no keyboard).
mountTokioStatsKeys();

// Auto-load on open when the original URL carried start_ns or bucket.
if (shouldAutoLoad(originalParams)) void loadAll();
