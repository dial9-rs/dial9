// The aggregated server-side flamegraph mode (`?api=1`): instead of fetching +
// decoding trace bytes client-side, the page opens the server's /api/flamegraph
// SSE stream and renders the pre-built tree each streamed snapshot carries. The
// server owns the refine/stop loop: it emits the already-folded snapshot first,
// then folds to the cap and pushes a fresh tree per file, closing the stream
// when done.

import {
  Dial9Creds,
  Dial9Session,
  applyToCreds,
  decodeFlamegraphTree,
  formatCoverageBadge,
  formatHumanDuration,
  hostFacetOptions,
  isSourceShareable,
  nextMaxFiles,
  nsToPickerUtc,
  openSse,
  pickerUtcToNs,
  readPlainSourceScope,
  refinementWorkDepth,
  shouldAdoptRefinementSnapshot,
  sourceScopeFromStored,
} from "../../lib/trace/index.js";
import type {
  ApiFlamegraphNode,
  Coverage,
  FlamegraphMetadata,
  FlamegraphNode,
  FlamegraphResponse,
} from "../../lib/trace/index.js";
import { createFlamegraph, diffSearch, fullScopeQuery } from "../../lib/canvas/index.js";
import { mountCopyLink } from "../../lib/url/index.js";
import type { PageEls } from "./dom.js";
import { closeHelpOnEscape, mountFlamegraphKeys } from "./fg-keys.js";
import type { FgKeys } from "./fg-keys.js";
import { createPollMinimap, type PollMinimap } from "./minimap.js";
import { buildApiUrl, buildBrowserQuery, seedFacetState, type ApiQueryState } from "./query.js";
import { createApiInspectSync } from "./view-state.js";

/** One accumulated facet: display label + monotonically-unioned values. */
interface AvailFacet {
  label: string;
  values: Set<string>;
}

export function runApiMode(params: URLSearchParams, els: PageEls): void {
  const { loadingEl, errorEl, containerEl, titleEl, statsEl } = els;
  const source = readPlainSourceScope(
    params,
    sourceScopeFromStored("", Dial9Creds.get()),
  );
  applyToCreds(source, Dial9Creds);

  // Literal credential URLs are useful for same-tab reloads but are not safe
  // built-in share targets. The private account-ID userscript owns that flow.
  const copyLink = mountCopyLink(els.headerEl);
  copyLink.el.style.display = isSourceShareable(source) ? "" : "none";

  // Filter toolbar. The facet controls are built from the backend's reported
  // metadata facets (see renderFacets), so only dimensions with data appear;
  // the time pickers and action buttons stay static.
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
  // "All" in the host selector re-applies this set; a named host narrows to
  // one. Kept separate from the live selection so "All" never broadens past the
  // original scope.
  const originalHosts = params.getAll("host");

  // Single source of truth for the query: seeded from the URL, mutated by the
  // facet controls, read by buildApiUrl / updateBrowserUrl. Decoupled from the
  // DOM so the first poll can fire before the facet selects exist (the picker
  // times are the exception: read from the DOM per build).
  const facetState = seedFacetState(params);
  let hosts: string[] = originalHosts.slice();

  // Initialize datetime pickers from URL ns params. The picker shows UTC
  // wall-clock; the read (ns -> picker) and write (picker -> ns) sides must be
  // symmetric, both treating the picker value as UTC.
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
  const scopeBucket = source.bucket || params.get("bucket");
  const scopePrefix = params.get("prefix");

  // Span-type filter from a Span Explorer deep link. Fixed for the life of the
  // view (no toolbar control), so read once alongside the other scope params.
  const scopeSpanTypeUid = params.get("span_type_uid");
  const scopeMinSpanNs = params.get("min_span_ns");
  const scopeMaxSpanNs = params.get("max_span_ns");

  // The poll-duration band minimap (aggregated mode). Seeded from the URL,
  // read into the query via minimap.band(); a brush/Apply re-streams. Assigned
  // below (before the first startStreaming), so queryState always sees it.
  let minimap: PollMinimap | undefined;

  function queryState(): ApiQueryState {
    const band = minimap?.band() ?? { minPollNs: null, maxPollNs: null };
    return {
      dataDir,
      source: { ...source, bucket: scopeBucket || source.bucket },
      prefix: scopePrefix,
      service: scopeService,
      hosts,
      facets: facetState,
      startNs: pickerUtcToNs(fStart.value),
      endNs: pickerUtcToNs(fEnd.value),
      minPollNs: band.minPollNs,
      maxPollNs: band.maxPollNs,
      maxFiles,
      spanTypeUid: scopeSpanTypeUid,
      minSpanNs: scopeMinSpanNs,
      maxSpanNs: scopeMaxSpanNs,
    };
  }

  function updateBrowserUrl(): void {
    const nextParams = new URLSearchParams(buildBrowserQuery(queryState()));
    inspectSync.carryTo(nextParams);
    history.pushState(
      null,
      "",
      window.location.pathname + "?" + nextParams.toString()
    );
  }

  // Build the data-driven facet controls from the backend's reported metadata
  // facets. Called on every response, but only rebuilds when the *available*
  // facet set changes (which also avoids clobbering a <select> mid-interaction).
  // The backend's facet metadata is scoped to the current query, so narrowing
  // one dimension collapses the others; we union every response's facets into
  // monotonically-growing sets so a control never loses an option it once
  // offered (otherwise selecting a host would make the host selector vanish and
  // you could never get back to "All"). Seeded with the original scope hosts so
  // the selector is correct before the first response.
  const availFacets: Record<string, AvailFacet> = {};
  let renderedFacetKey: string | null = null;
  function renderFacets(meta: FlamegraphMetadata): void {
    for (const f of meta.facets || []) {
      const acc = (availFacets[f.name] ??= { label: f.label, values: new Set() });
      for (const v of f.values) acc.values.add(v);
    }
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
      startStreaming();
    }

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

  // Render the scope summary in the page header from the backend's resolved
  // scope (service, host(s), time range), so the header reflects backend truth
  // rather than the URL params the client guessed at.
  function renderScopeHeader(meta: FlamegraphMetadata): void {
    const bits: string[] = [];
    // `host_names` is not part of the current wire contract (the server sends
    // `hosts` as a count); read tolerantly for older/other servers, so with the
    // current server this branch never fires.
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

  // Convert the wire tree (children as an optional array) into the widget's
  // shape (children as a Map).
  function toFgTree(node: ApiFlamegraphNode): FlamegraphNode {
    const m = new Map<string, FlamegraphNode>();
    for (const child of node.children || []) {
      m.set(child.name, toFgTree(child));
    }
    return { name: node.name, count: node.count, self: node.self, children: m };
  }

  // Canvas zoom is not URL-synced in api mode. Inspection is, using its stable
  // frame identity so a shared link can restore it as snapshots arrive.
  let keys: FgKeys | null = null;
  const inspectSync = createApiInspectSync(params, () => fg);
  const fg = createFlamegraph(containerEl, () => {
    keys?.recordZoom();
    inspectSync.onViewChange();
  });
  // Unified keys: `?` help, `f` fit, `z` zoom-undo. The widget's toolbar +
  // canvases exist as of createFlamegraph, so the root state recorded at mount
  // is the correct undo baseline.
  keys = mountFlamegraphKeys({ fg, containerEl });

  // Build the base stats string from the response metadata, shared by every poll.
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

  // Stream state. Each open stream gets an AbortController; the next start
  // (filters change, "Refine more") or "Stop" aborts it. A monotonic
  // `streamToken` guards late callbacks from a stream that has since been
  // superseded.
  let streamToken = 0;
  let abortCtl: AbortController | null = null;
  // The last-rendered coverage badge (without the trailing status suffix), so
  // the close/stop/error handlers can re-stamp it. `lastCoverage` feeds the
  // "Refine more" ceiling computation.
  let lastBadge = "";
  let lastCoverage: Coverage | null = null;
  let gotEvent = false;
  const stopBtn = document.getElementById("f-stop") as HTMLButtonElement;
  const moreBtn = document.getElementById("f-more") as HTMLButtonElement;
  const applyBtn = document.getElementById("f-apply") as HTMLButtonElement;

  function setRefiningUi(active: boolean): void {
    // Stop button: enabled only while streaming.
    stopBtn.disabled = !active;
    stopBtn.style.opacity = active ? "1" : "0.4";
    stopBtn.style.cursor = active ? "pointer" : "not-allowed";
    // Refine-more button: enabled only when idle (stream closed).
    moreBtn.disabled = active;
    moreBtn.style.opacity = active ? "0.4" : "1";
    moreBtn.style.cursor = active ? "not-allowed" : "pointer";
  }

  function stopStreaming(): void {
    streamToken++;
    if (abortCtl != null) {
      abortCtl.abort();
      abortCtl = null;
    }
    setRefiningUi(false);
  }

  // Render one streamed snapshot (a full FlamegraphResponse).
  function renderEvent(resp: FlamegraphResponse): void {
    gotEvent = true;
    const meta = resp.metadata;
    renderScopeHeader(meta);
    renderFacets(meta);

    // Poll-duration minimap (#663). Guarded so a minimap failure cannot strand
    // the flamegraph.
    try {
      minimap?.render(meta.poll_duration_histogram);
    } catch (e) {
      console.warn("poll-duration minimap render failed:", e);
    }

    // First event hides the loading overlay; later events refine in-place
    // without it.
    loadingEl.classList.add("hidden");
    containerEl.style.display = "flex";
    const tree = decodeFlamegraphTree(resp);
    fg.setTreeDirect(toFgTree(tree), tree.count);

    const cov = resp.coverage;
    lastCoverage = cov ?? null;
    // Local-dir / non-aggregation mode carries no coverage badge.
    lastBadge =
      cov == null
        ? baseStats(resp)
        : baseStats(resp) + " \u00b7 " + formatCoverageBadge(cov);
    statsEl.innerHTML =
      lastBadge +
      ' \u00b7 <span class="spinner" style="width:0.9em;height:0.9em;display:inline-block;vertical-align:middle"></span> refining\u2026';
  }

  // (Re)start the stream. `preserveExisting` marks a SAME-SCOPE deepening
  // ("Refine more"): cached state is reconstructed from bounded seed batches,
  // so snapshots shallower than what is already rendered are ignored rather
  // than allowed to momentarily shrink the tree, the full-page loading overlay
  // is skipped, and the live zoom/inspect view is restored across the swap.
  // A fresh scope adopts everything and resets.
  function startStreaming(preserveExisting = false): void {
    inspectSync.preserveForTreeChange();
    const baselineFilesFolded =
      preserveExisting && lastCoverage ? lastCoverage.files_folded : 0;
    stopStreaming();
    setRefiningUi(true);
    gotEvent = false;
    errorEl.style.display = "none";
    if (preserveExisting) {
      statsEl.innerHTML =
        lastBadge +
        ' \u00b7 <span class="spinner" style="width:0.9em;height:0.9em;display:inline-block;vertical-align:middle"></span> refining\u2026';
    } else {
      loadingEl.classList.remove("hidden");
      loadingEl.innerHTML = '<div class="spinner"></div>Loading aggregated flamegraph\u2026';
      containerEl.style.display = "none";
    }
    const token = ++streamToken;
    abortCtl = new AbortController();
    void openSse(buildApiUrl(queryState(), window.location.origin), {
      headers: Dial9Session.headers(Dial9Creds.headers()),
      signal: abortCtl.signal,
      onEvent: (obj) => {
        if (token !== streamToken) return;
        const resp = obj as FlamegraphResponse;
        if (
          !shouldAdoptRefinementSnapshot(
            preserveExisting,
            baselineFilesFolded,
            resp.coverage?.files_folded ?? 0,
          )
        ) {
          return;
        }
        // setTreeDirect resets the canvas view, so carry the live zoom across
        // a same-scope refine - the user did not ask to be moved.
        const preservedView = preserveExisting ? fg.getViewState() : null;
        renderEvent(resp);
        if (preservedView) fg.applyViewState(preservedView);
        inspectSync.restoreAfterTreeChange();
      },
      onClose: () => {
        if (token !== streamToken) return;
        // Cached reconstruction and the bounded work are both complete.
        statsEl.textContent =
          lastBadge +
          (gotEvent ? " \u00b7 refined" : preserveExisting ? " \u00b7 refinement incomplete" : "");
        setRefiningUi(false);
      },
      onError: (err) => {
        if (token !== streamToken) return;
        if (preserveExisting) {
          // Reconstruction is best-effort; keep the same-scope tree on screen.
          statsEl.textContent = lastBadge + " \u00b7 refinement interrupted";
        } else if (!gotEvent) {
          els.showError("Failed to load flamegraph: " + err.message);
        } else {
          // Mid-stream drop: keep the rendered tree but clear the
          // (would-be-eternal) spinner.
          statsEl.textContent = lastBadge + " \u00b7 interrupted";
        }
        setRefiningUi(false);
      },
    });
  }

  applyBtn.addEventListener("click", () => {
    // New filters invalidate the current scope; reset depth.
    maxFiles = null;
    updateBrowserUrl();
    startStreaming();
  });

  moreBtn.addEventListener("click", () => {
    // Deeper sampling for the current scope. Grow from the BOUNDED WORK the
    // server was allowed this request, not from how many cached files the
    // snapshot covers: an all-cache scope reports a deep files_folded that
    // would otherwise jump straight to the ceiling in a single click.
    maxFiles = nextMaxFiles(refinementWorkDepth(lastCoverage, maxFiles));
    updateBrowserUrl();
    startStreaming(true); // same scope -> keep the tree and the live view
  });

  stopBtn.addEventListener("click", () => {
    // Manual stop: abort the stream, freeze at the current coverage.
    stopStreaming();
    statsEl.textContent = lastBadge + " \u00b7 stopped";
  });

  // Poll-duration minimap: built after the toolbar, seeded from the URL band
  // params. A brush/Apply resets the fold ceiling, rewrites the browser URL and
  // re-streams (like a facet change); the fast/slow split opens a two-sided diff.
  minimap = createPollMinimap({
    anchor: toolbar,
    ctlStyle,
    initialMinPollNs: params.get("min_poll_ns"),
    initialMaxPollNs: params.get("max_poll_ns"),
    onApply: () => {
      maxFiles = null;
      updateBrowserUrl();
      startStreaming();
    },
    onDiffFastSlow: (splitNs) => {
      // A = fast polls (< split), B = slow polls (>= split). Server band
      // bounds are inclusive, so the fast side stops one ns below the split to
      // keep the partition disjoint. Opens the two-sided diff in a new tab.
      const base = () => fullScopeQuery(new URLSearchParams(buildBrowserQuery(queryState())));
      const a = base();
      a.delete("min_poll_ns");
      a.set("max_poll_ns", String(splitNs - 1));
      const b = base();
      b.set("min_poll_ns", String(splitNs));
      b.delete("max_poll_ns");
      window.open(window.location.pathname + "?" + diffSearch(a, b), "_blank");
    },
  });

  startStreaming();
  window.addEventListener("resize", () => fg.resize());
  // Escape cascade: an open unified help overlay closes first (no-op when closed).
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      if (keys !== null && closeHelpOnEscape(keys)) return;
      fg.handleEscape();
    }
  });
}
