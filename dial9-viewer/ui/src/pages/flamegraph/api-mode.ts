// src/pages/flamegraph/api-mode.ts - the aggregated server-side mode
// (`?api=1`; features/03 section P, F168-F185): a behavior-preserving
// typed port of the legacy inline bootstrap's api branch
// (flamegraph.html:112-501). Instead of fetching + decoding trace bytes
// client-side, the page polls the server's demand-driven /api/flamegraph
// refinement loop and renders the pre-built tree each response carries.
//
// The pure helpers (coverage badge/percent, plateau auto-stop, UTC picker
// conversion, max_files ceiling, host facet options) are the SAME
// implementation the legacy page runs - flamegraph_api.js via the
// lib/trace seam - so the two page generations cannot drift while both
// are servable. The raw fetch loop is ported verbatim (rather than the
// lib/trace aggregates client) because the legacy page surfaces a 404 as
// an error while the client maps it to "unavailable"; parity wins.

import {
  coveragePercent,
  Dial9Creds,
  formatCoverageBadge,
  formatHumanDuration,
  hostFacetOptions,
  isCoverageFrozen,
  nextMaxFiles,
  nsToPickerUtc,
  pickerUtcToNs,
  shouldAutoStopRefining,
} from "../../lib/trace/index.js";
import type {
  ApiFlamegraphNode,
  Coverage,
  FlamegraphMetadata,
  FlamegraphNode,
  FlamegraphResponse,
} from "../../lib/trace/index.js";
import { createFlamegraph } from "../../lib/canvas/index.js";
import { mountCopyLink } from "../../lib/url/index.js";
import type { PageEls } from "./dom.js";
import { buildApiUrl, buildBrowserQuery, seedFacetState, type ApiQueryState } from "./query.js";

/** One accumulated facet: display label + monotonically-unioned values. */
interface AvailFacet {
  label: string;
  values: Set<string>;
}

export function runApiMode(params: URLSearchParams, els: PageEls): void {
  const { loadingEl, errorEl, containerEl, titleEl, statsEl } = els;

  // Copy-link (T19). No beforeCopy flush here: api mode has no debounced
  // view-state sync - the URL is already current, because Apply/facet
  // changes pushState it synchronously (F180) and canvas zoom is
  // deliberately NOT URL-synced in this mode (legacy parity).
  mountCopyLink(els.headerEl);

  // --- Filter toolbar (data-driven) ---
  // The facet controls (Thread / Source / Host) are NOT hard-coded:
  // they're built from the backend's reported `metadata` facets on the
  // first response (see renderFacets), so the toolbar offers only
  // dimensions that actually have data. The time pickers and action
  // buttons are intrinsic, so they stay static.
  const ctlStyle =
    "background:#1a1a2e;color:#e0e0e0;border:1px solid #444;padding:2px 6px;border-radius:3px";
  const btnStyle =
    "border:1px solid #444;padding:4px 14px;border-radius:3px;cursor:pointer;font-weight:600";
  const toolbar = document.createElement("div");
  toolbar.style.cssText =
    "display:flex;align-items:center;gap:12px;padding:6px 12px;background:#16213e;border-bottom:1px solid #333;font-size:0.8em;flex-shrink:0;flex-wrap:wrap;";
  toolbar.innerHTML = `
        <span id="f-facets" style="display:flex;align-items:center;gap:12px;flex-wrap:wrap"></span>
        <label style="color:#e0e0e0">From: <input id="f-start" type="datetime-local" step="1" style="${ctlStyle}"></label>
        <label style="color:#e0e0e0">To: <input id="f-end" type="datetime-local" step="1" style="${ctlStyle}"></label>
        <button id="f-apply" style="background:#6c63ff;color:#fff;border:none;padding:4px 14px;border-radius:3px;cursor:pointer;font-weight:600">Apply</button>
        <button id="f-more" style="background:#2a2a4a;color:#e0e0e0;${btnStyle}">Refine more</button>
        <button id="f-stop" style="background:#2a2a4a;color:#e0e0e0;${btnStyle};opacity:0.4;cursor:not-allowed" disabled>Stop</button>
    `;
  const pageHeader = document.querySelector(".fg-page-header");
  if (pageHeader === null) {
    throw new Error("flamegraph shell is missing .fg-page-header");
  }
  pageHeader.after(toolbar);

  const fStart = document.getElementById("f-start") as HTMLInputElement;
  const fEnd = document.getElementById("f-end") as HTMLInputElement;

  // The original scope host set from the URL (the heatmap box selection).
  // "All" in the host selector re-applies this set; a named host narrows
  // to one. Kept separate from the live selection so "All" never broadens
  // past the original scope.
  const originalHosts = params.getAll("host");

  // Single source of truth for the query. Seeded from the URL, mutated by
  // the facet controls, read by buildApiUrl / updateBrowserUrl.
  // Deliberately decoupled from the DOM so the first poll can fire before
  // the (data-driven) facet selects even exist. (The picker times are the
  // exception: read from the DOM per build, like legacy.)
  const facetState = seedFacetState(params);
  let hosts: string[] = originalHosts.slice();

  // Initialize datetime pickers from URL ns params. The picker shows UTC
  // wall-clock (S3 trace keys are bucketed in UTC). The read (ns ->
  // picker) and write (picker -> ns) sides MUST be symmetric: both treat
  // the picker value as UTC. See the unit-tested nsToPickerUtc /
  // pickerUtcToNs in flamegraph_api.js.
  const startNsParam = params.get("start_ns");
  const endNsParam = params.get("end_ns");
  if (startNsParam) fStart.value = nsToPickerUtc(startNsParam);
  if (endNsParam) fEnd.value = nsToPickerUtc(endNsParam);

  // `max_files` ceiling override, raised by the "Refine more" button.
  // null = let the backend pick its own ceiling.
  const maxFilesParam = params.get("max_files");
  let maxFiles: number | null = maxFilesParam ? Number(maxFilesParam) : null;

  // Scope dimensions that define WHICH files the server-side loop samples
  // (the box's service; hosts live alongside facetState above).
  const dataDir = params.get("data_dir");
  const scopeService = params.get("service");
  const scopeBucket = params.get("bucket");
  const scopePrefix = params.get("prefix");

  function queryState(): ApiQueryState {
    return {
      dataDir,
      bucket: scopeBucket,
      prefix: scopePrefix,
      service: scopeService,
      hosts,
      facets: facetState,
      startNs: pickerUtcToNs(fStart.value),
      endNs: pickerUtcToNs(fEnd.value),
      maxFiles,
    };
  }

  function updateBrowserUrl(): void {
    history.pushState(
      null,
      "",
      window.location.pathname + "?" + buildBrowserQuery(queryState())
    );
  }

  // Build the data-driven facet controls from the backend's reported
  // metadata facets. Called on every response, but only rebuilds when the
  // *available* facet set changes (the facets are recorded independent of
  // the active filter, so they stay stable as the user flips selectors).
  // The guard also avoids clobbering a <select> mid-interaction.
  // Accumulated available facets: the backend's facet metadata is scoped
  // to the *current* query, so narrowing one dimension collapses the
  // others (picking a single host makes that response report only that
  // host, only its sources, etc.). We union every response's facets into
  // monotonically-growing sets, so a control never loses an option it
  // once offered - otherwise selecting a host would make the host
  // selector vanish and you could never get back to "All". Seeded with
  // the original scope hosts so the selector is correct even before the
  // first response.
  const availFacets: Record<string, AvailFacet> = {};
  let renderedFacetKey: string | null = null;
  function renderFacets(meta: FlamegraphMetadata): void {
    // Accumulate from the generic facets array.
    for (const f of meta.facets || []) {
      const acc = (availFacets[f.name] ??= { label: f.label, values: new Set() });
      for (const v of f.values) acc.values.add(v);
    }
    // Seed the host facet from the original scope hosts.
    const hostFacet = availFacets["host"];
    if (hostFacet) {
      for (const h of originalHosts) hostFacet.values.add(h);
    }

    const key = JSON.stringify(
      Object.fromEntries(
        Object.entries(availFacets).map(([k, v]) => [k, [...v.values].sort()])
      )
    );
    if (key === renderedFacetKey) return;
    renderedFacetKey = key;

    const container = document.getElementById("f-facets");
    if (container === null) return;
    container.innerHTML = "";

    function addSelect(
      labelText: string,
      options: readonly { value: string; label: string }[],
      selected: string,
      onChange: (value: string) => void
    ): void {
      const label = document.createElement("label");
      label.style.color = "#e0e0e0";
      label.appendChild(document.createTextNode(labelText + " "));
      const sel = document.createElement("select");
      sel.style.cssText = ctlStyle;
      for (const opt of options) {
        const o = document.createElement("option");
        o.value = opt.value;
        o.textContent = opt.label;
        if (opt.value === selected) o.selected = true;
        sel.appendChild(o);
      }
      sel.addEventListener("change", () => onChange(sel.value));
      label.appendChild(sel);
      if (container !== null) container.appendChild(label);
    }

    function applyFacetChange(): void {
      maxFiles = null;
      updateBrowserUrl();
      startPolling();
    }

    // Render each facet generically.
    for (const [name, { label, values }] of Object.entries(availFacets)) {
      const sorted = [...values].sort();
      if (name === "host") {
        if (sorted.length > 1) {
          const first = hosts[0];
          const selectedHost = hosts.length === 1 && first !== undefined ? first : "";
          addSelect(label + ":", hostFacetOptions(sorted), selectedHost, (v) => {
            hosts = v ? [v] : originalHosts.slice();
            applyFacetChange();
          });
        }
        continue;
      }
      if (sorted.length <= 1) continue;
      const opts = [
        { value: "", label: "All" },
        ...sorted.map((v) => ({ value: v, label: v })),
      ];
      const current = facetState[name] || "";
      addSelect(label + ":", opts, current, (v) => {
        facetState[name] = v;
        applyFacetChange();
      });
    }
  }

  // Render the scope summary in the page header from the backend's
  // resolved scope + facets (service, host(s), time range), so the header
  // reflects backend truth rather than URL params the client guessed at.
  function renderScopeHeader(meta: FlamegraphMetadata): void {
    const bits: string[] = [];
    // `host_names` is not part of the current wire contract
    // (src/server/flamegraph.rs sends `hosts` as a count) - the legacy
    // page reads it tolerantly for older/other servers and so does this
    // port; with the current server the branch never fires.
    const hostNames =
      (meta as FlamegraphMetadata & { host_names?: string[] }).host_names || [];
    const firstHost = hostNames[0];
    if (hostNames.length === 1 && firstHost !== undefined) bits.push(firstHost);
    else if (hostNames.length > 1) bits.push(`${hostNames.length} hosts`);
    if (meta.min_timestamp_ns && meta.max_timestamp_ns) {
      const from =
        new Date(meta.min_timestamp_ns / 1e6).toISOString().slice(0, 19) + "Z";
      const to =
        new Date(meta.max_timestamp_ns / 1e6).toISOString().slice(11, 19) + "Z";
      bits.push(`${from} \u2192 ${to}`);
    }
    const svc = meta.service || "aggregated";
    document.title = `Flamegraph \u2014 ${svc}`;
    titleEl.textContent = bits.length
      ? `Flamegraph \u2014 ${svc} \u00b7 ${bits.join(" \u00b7 ")}`
      : `Flamegraph \u2014 ${svc}`;
  }

  // Convert the wire tree (children as an optional array) into the frozen
  // widget's shape (children as a Map).
  function toFgTree(node: ApiFlamegraphNode): FlamegraphNode {
    const m = new Map<string, FlamegraphNode>();
    for (const child of node.children || []) {
      m.set(child.name, toFgTree(child));
    }
    return { name: node.name, count: node.count, self: node.self, children: m };
  }

  // No updateUrlZoom callback: canvas zoom is NOT URL-synced in api mode
  // (F180) - the browser URL carries the filter scope instead.
  const fg = createFlamegraph(containerEl);

  // Build the base stats string ("N samples [middot] M hosts [middot] ...")
  // from the response metadata, shared by every poll.
  function baseStats(resp: FlamegraphResponse): string {
    const meta = resp.metadata;
    const bits = [`${resp.total_samples.toLocaleString()} samples`];
    if (meta.hosts > 1) bits.push(`${meta.hosts} hosts`);
    if (meta.min_timestamp_ns && meta.max_timestamp_ns) {
      const from = new Date(meta.min_timestamp_ns / 1e6);
      const to = new Date(meta.max_timestamp_ns / 1e6);
      const dur = formatHumanDuration(meta.max_timestamp_ns - meta.min_timestamp_ns);
      bits.push(
        `${from.toISOString().slice(11, 19)} \u2192 ${to
          .toISOString()
          .slice(11, 19)} (${dur})`
      );
    }
    return bits.join(" \u00b7 ");
  }

  // Poll-loop state. `pollToken` lets us cancel an in-flight loop when
  // the filters change or "Refine more"/"Stop" is clicked: each loop
  // captures the token and bails if it no longer matches the current one.
  const POLL_INTERVAL_MS = 800;
  let pollToken = 0;
  let pollTimer: ReturnType<typeof setTimeout> | null = null;
  let prevCoverage: Coverage | null = null;
  // Per-poll coverage gains (percentage points), newest last. Drives the
  // auto-stop heuristic (shouldAutoStopRefining).
  let coverageDeltas: number[] = [];
  const stopBtn = document.getElementById("f-stop") as HTMLButtonElement;
  const moreBtn = document.getElementById("f-more") as HTMLButtonElement;
  const applyBtn = document.getElementById("f-apply") as HTMLButtonElement;

  function setRefiningUi(active: boolean): void {
    // Stop button: enabled only while refining.
    stopBtn.disabled = !active;
    stopBtn.style.opacity = active ? "1" : "0.4";
    stopBtn.style.cursor = active ? "pointer" : "not-allowed";
    // Refine-more button: enabled only when idle (not refining).
    moreBtn.disabled = active;
    moreBtn.style.opacity = active ? "0.4" : "1";
    moreBtn.style.cursor = active ? "not-allowed" : "pointer";
  }

  function stopPolling(): void {
    pollToken++;
    if (pollTimer != null) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
    setRefiningUi(false);
  }

  // `refine` is false on the first poll for a scope (read-only: the
  // server returns whatever is already folded, instantly), then true to
  // fold a batch each subsequent poll.
  async function poll(token: number, refine: boolean): Promise<void> {
    let resp: FlamegraphResponse;
    try {
      const credHeaders = Dial9Creds.headers();
      const httpResp = await fetch(
        buildApiUrl(queryState(), refine, window.location.origin),
        { headers: credHeaders }
      );
      if (!httpResp.ok) throw new Error(await httpResp.text());
      resp = (await httpResp.json()) as FlamegraphResponse;
    } catch (err) {
      if (token !== pollToken) return; // superseded
      const message = err instanceof Error ? err.message : String(err);
      els.showError("Failed to load flamegraph: " + message);
      return;
    }
    if (token !== pollToken) return; // superseded while fetching

    const meta = resp.metadata;
    // Header scope summary + filter controls are both built from the
    // backend's reported metadata (data-driven), not hard-coded HTML.
    renderScopeHeader(meta);
    renderFacets(meta);

    // First successful response hides the loading overlay; subsequent
    // polls refine in-place without it.
    loadingEl.classList.add("hidden");
    containerEl.style.display = "flex";
    fg.setTreeDirect(toFgTree(resp.tree), resp.tree.count);

    const cov = resp.coverage;
    if (cov == null) {
      // Older / local-dir mode: no coverage, single fetch only (F183).
      statsEl.textContent = baseStats(resp);
      stopPolling();
      return;
    }

    // Track per-poll coverage gain (skip the read-only first poll, which
    // establishes the baseline rather than refining).
    if (refine && prevCoverage != null) {
      coverageDeltas.push(coveragePercent(cov) - coveragePercent(prevCoverage));
    }
    const frozen = isCoverageFrozen(prevCoverage, cov);
    const plateaued = shouldAutoStopRefining(coverageDeltas);
    prevCoverage = cov;

    const badge = baseStats(resp) + " \u00b7 " + formatCoverageBadge(cov);

    // Stop when the server can't fold more (frozen) or coverage has
    // plateaued (diminishing returns over recent polls) - F177.
    if (refine && (frozen || plateaued)) {
      statsEl.textContent = badge + " \u00b7 refined";
      stopPolling();
      return;
    }
    statsEl.innerHTML =
      badge +
      ' \u00b7 <span class="spinner" style="width:0.9em;height:0.9em;display:inline-block;vertical-align:middle"></span> refining\u2026';

    // Subsequent polls refine. Schedule the next one.
    setRefiningUi(true);
    pollTimer = setTimeout(() => {
      if (token === pollToken) void poll(token, true);
    }, POLL_INTERVAL_MS);
  }

  // (Re)start the poll loop from scratch: cancels any in-flight loop,
  // resets freeze tracking, and shows the loading overlay. The first poll
  // is read-only (instant); refinement kicks in on the polls that follow.
  function startPolling(): void {
    stopPolling();
    prevCoverage = null;
    coverageDeltas = [];
    setRefiningUi(true);
    loadingEl.classList.remove("hidden");
    loadingEl.innerHTML = '<div class="spinner"></div>Loading aggregated flamegraph\u2026';
    containerEl.style.display = "none";
    errorEl.style.display = "none";
    const token = pollToken;
    void poll(token, false);
  }

  applyBtn.addEventListener("click", () => {
    // New filters invalidate the current scope; reset depth (F172).
    maxFiles = null;
    updateBrowserUrl();
    startPolling();
  });

  moreBtn.addEventListener("click", () => {
    // Request deeper sampling for the current scope: raise the ceiling to
    // ~4x the files folded so far, then resume refining (reset the
    // plateau tracker so it doesn't immediately auto-stop on the prior
    // history) - F178.
    const folded = prevCoverage ? prevCoverage.files_folded : 0;
    maxFiles = nextMaxFiles(folded);
    coverageDeltas = [];
    stopPolling();
    setRefiningUi(true);
    // Show the refining spinner immediately in the stats bar.
    statsEl.innerHTML =
      (statsEl.textContent ?? "").replace(/ \u00b7 (?:refined|stopped)$/, "") +
      ' \u00b7 <span class="spinner" style="width:0.9em;height:0.9em;display:inline-block;vertical-align:middle"></span> refining\u2026';
    const token = pollToken;
    void poll(token, true);
  });

  stopBtn.addEventListener("click", () => {
    // Manual stop: freeze at the current coverage (F179).
    const badge = (statsEl.textContent ?? "").replace(/ \u00b7 refining\u2026$/, "");
    stopPolling();
    statsEl.textContent = badge + " \u00b7 stopped";
  });

  startPolling();
  window.addEventListener("resize", () => fg.resize());
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") fg.handleEscape();
  });
}
