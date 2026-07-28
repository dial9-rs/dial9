// The detail panel for the selected span type: header, duration histogram,
// band readout, five-way time composition, flamegraph deep links, exemplars,
// and the coverage badge.

import { html, nothing, render, type TemplateResult } from "lit-html";
import {
  TIME_CATEGORIES,
  computeTimeComposition,
  countInBand,
  exemplarViewerUrl,
  exemplarsInBand,
  flamegraphUrl,
  fmtNs,
  formatCoverageBadge,
  spanTypeLabel,
  spanTypeQuality,
} from "../../lib/trace/index.js";
import type {
  AttrFilter,
  Coverage,
  DurationBand,
  Exemplar,
  ExemplarLinkScope,
  SpanExplorerState,
  SpanTypeStats,
} from "../../lib/trace/index.js";
import { renderHistogram } from "./histogram.js";
import { exemplarsTemplate, type ExemplarTableCtx } from "./exemplars.js";
import type { ColumnOverrides } from "./columns.js";

const EMPTY_HINT =
  "Select a span type above to see its duration histogram, time composition, and flamegraphs.";

/** Everything the panel renders from. */
export interface DetailModel {
  spanType: SpanTypeStats | undefined;
  band: DurationBand;
  coverage: Coverage | null;
  attrFilters: readonly AttrFilter[];
  overrides: ColumnOverrides;
  rawMode: boolean;
  /** An exemplar refresh is in flight, so counts/rows are not yet authoritative. */
  exemplarRefreshPending: boolean;
  /** A cumulative preview of the in-flight refresh has already been adopted. */
  exemplarPreviewAvailable: boolean;
  /** Scope for the flamegraph deep links (null in raw mode). */
  linkState: SpanExplorerState | null;
  onBand: (band: DurationBand) => void;
  onClearBand: () => void;
  onToggleFilter: (key: string, value: string) => void;
  onSetOverride: (id: string, value: boolean | undefined) => void;
  onResetOverrides: () => void;
}

/** The link scope: page identity plus the type's name for `focus_span_name`. */
function linkScope(m: DetailModel): ExemplarLinkScope {
  return {
    bucket: m.linkState?.bucket ?? null,
    region: m.linkState?.region ?? null,
    service: m.linkState?.service ?? null,
    spanName: m.spanType?.name ?? null,
  };
}

function headerTemplate(st: SpanTypeStats, m: DetailModel, slowest: Exemplar | null): TemplateResult {
  // The header's `max` is an all-instances statistic, so it only becomes a jump
  // link when no band is active - an in-band exemplar's duration may differ from
  // the displayed maximum.
  const hasBand = m.band.min_ns != null || m.band.max_ns != null;
  const maxJumpUrl =
    !hasBand && slowest && !m.rawMode ? exemplarViewerUrl(slowest, linkScope(m)) : "";
  return html`<div class="detail-header">
    <h2>${spanTypeLabel(st)}</h2>
    <span class="meta"
      >${st.count.toLocaleString()} instances · p50 ${fmtNs(st.p50_ns)} · p99
      ${fmtNs(st.p99_ns)} · max
      ${maxJumpUrl
        ? html`<a
            class="max-jump"
            href=${maxJumpUrl}
            target="_blank"
            title="Jump to the slowest instance of this span in the viewer"
            >${fmtNs(st.max_ns)}</a
          >`
        : fmtNs(st.max_ns)}</span
    >
  </div>`;
}

function bandTemplate(st: SpanTypeStats, m: DetailModel): TemplateResult {
  if (m.band.min_ns == null && m.band.max_ns == null) {
    return html`<div class="band-info">
      <span style="color:#666">Drag the histogram to select a duration band for flamegraphs.</span>
    </div>`;
  }
  // The server's exact in-band count wins when it has arrived; otherwise the
  // histogram estimate stands in.
  const count =
    st.selected_duration_count != null
      ? Number(st.selected_duration_count)
      : countInBand(st.histogram, m.band.min_ns, m.band.max_ns);
  return html`<div class="band-info">
    <span
      >Selected band: ${m.band.min_ns != null ? fmtNs(m.band.min_ns) : "0"} –
      ${m.band.max_ns != null ? fmtNs(m.band.max_ns) : "∞"}</span
    >
    <span class="count"
      >${m.exemplarRefreshPending
        ? "refreshing exact count…"
        : `${count.toLocaleString()} instances`}</span
    >
    <button
      class="secondary"
      style="font-size:0.85em;padding:2px 8px;cursor:pointer"
      @click=${m.onClearBand}
    >
      Clear
    </button>
  </div>`;
}

function compositionTemplate(st: SpanTypeStats, m: DetailModel): TemplateResult {
  const hasBand = m.band.min_ns != null || m.band.max_ns != null;
  const bandScoped = hasBand && (st.composition_histogram?.length ?? 0) > 0;
  const comp = computeTimeComposition(st, m.band);
  const quality = spanTypeQuality(st);
  const byKey = new Map(comp.categories.map((c) => [c.key, c]));

  // Equal weighting is the key caveat worth stating: shares are the mean
  // per-instance split, so a few very long spans cannot dominate.
  const scopeWord = bandScoped ? ", selected band" : "";
  const weightWord = comp.weighting === "equal" ? ", per-instance avg" : "";
  const isFallback =
    comp.categories.length === 1 && comp.categories[0]?.key === "unknown" && !st.composition;

  let badge: string | null = null;
  if (isFallback) badge = "awaiting per-category data";
  // A band is selected but this type has no per-bucket data, so composition is
  // still the all-instances total. Say so rather than let it read as scoped.
  else if (hasBand && !bandScoped) badge = "all instances";
  else if (quality != null && quality < 1.0) badge = `${(quality * 100).toFixed(0)}% detailed`;

  return html`<div class="composition-wrap">
    <div style="font-size:0.8em;color:#aaa;margin-bottom:4px;font-weight:600">
      Time composition (estimated${scopeWord}${weightWord})${badge
        ? html`<span class="partial-badge">${badge}</span>`
        : nothing}
    </div>
    <div class="composition-bar">
      ${comp.categories
        .filter((c) => c.frac > 0)
        .map(
          (c) => html`<div
            class="seg"
            style="width:${(c.frac * 100).toFixed(1)}%;background:${c.color}"
            title="${c.label}: ${fmtNs(c.ns)} (${(c.frac * 100).toFixed(1)}%)"
          ></div>`,
        )}
    </div>
    <div class="composition-legend">
      ${TIME_CATEGORIES.map((cat) => {
        // Zero-time categories dim rather than disappear, so the legend's shape
        // stays stable as the band moves.
        const c = byKey.get(cat.key);
        const frac = c?.frac ?? 0;
        const ns = c?.ns ?? 0;
        const meanNs = c?.meanNs ?? null;
        const pct = (frac * 100).toFixed(frac > 0 && frac < 0.001 ? 2 : 1);
        // The mean-per-instance duration matches the equal-weighted share, so
        // prefer it; the total is always in the tooltip.
        const primaryNs = meanNs != null ? meanNs : ns;
        const primaryLabel = meanNs != null ? `${fmtNs(meanNs)}/span` : fmtNs(ns);
        const title =
          meanNs != null
            ? `${cat.label}: ${pct}% · ${fmtNs(meanNs)} per span · ${fmtNs(ns)} total`
            : `${cat.label}: ${pct}% · ${fmtNs(ns)} total`;
        return html`<span class="item" style=${frac <= 0 ? "opacity:0.4" : ""} title=${title}>
          <span class="swatch" style="background:${cat.color}"></span>
          ${cat.label} <span class="legend-pct">${pct}%</span>
          ${primaryNs > 0 ? html`<span class="legend-ns">(${primaryLabel})</span>` : nothing}
        </span>`;
      })}
    </div>
  </div>`;
}

/**
 * Open the flamegraph from the click handler rather than relying on
 * `target=_blank`: modern anchors may imply `noopener`, which stops the
 * same-origin tab from inheriting sessionStorage-backed BYOC credentials and
 * yields a misleading 404 after S3 rejects every prefix listing.
 */
function openWithSession(e: Event): void {
  e.preventDefault();
  const href = (e.currentTarget as HTMLAnchorElement).href;
  if (!window.open(href, "_blank")) window.location.href = href;
}

function flamegraphLinksTemplate(m: DetailModel): TemplateResult {
  if (m.rawMode || m.linkState == null) {
    return html`<div class="fg-links">
      <span style="color:#666;font-size:0.82em;padding:6px 0"
        >🔥 On-CPU and 🧱 Blocking flamegraphs require the aggregation backend (not available
        in raw trace mode).</span
      >
    </div>`;
  }
  return html`<div class="fg-links">
    <a
      class="on-cpu"
      href=${flamegraphUrl(m.linkState, "cpu")}
      target="_blank"
      @click=${openWithSession}
      >🔥 On-CPU Flamegraph</a
    >
    <a
      class="blocking"
      href=${flamegraphUrl(m.linkState, "blocking")}
      target="_blank"
      @click=${openWithSession}
      >🧱 Blocking Flamegraph</a
    >
  </div>`;
}

/** Render the detail panel. The histogram is filled in imperatively after. */
export function renderDetail(detailPanel: HTMLElement, m: DetailModel): void {
  const st = m.spanType;
  if (!st) {
    detailPanel.className = "detail-panel empty";
    render(html`<span>${EMPTY_HINT}</span>`, detailPanel);
    return;
  }
  detailPanel.className = "detail-panel";

  // While a refresh is in flight and nothing has been previewed yet, the cached
  // rows belong to the PREVIOUS bounds - showing them would relabel another
  // band's instances as this one's.
  const exemplars =
    m.exemplarRefreshPending && !m.exemplarPreviewAvailable
      ? []
      : exemplarsInBand(st.exemplars, m.band.min_ns, m.band.max_ns);
  const hasBand = m.band.min_ns != null || m.band.max_ns != null;

  const tableCtx: ExemplarTableCtx = {
    spanType: st,
    scope: linkScope(m),
    attrFilters: m.attrFilters,
    overrides: m.overrides,
    rawMode: m.rawMode,
    onToggleFilter: m.onToggleFilter,
    onSetOverride: m.onSetOverride,
    onResetOverrides: m.onResetOverrides,
  };

  render(
    html`
      ${headerTemplate(st, m, exemplars[0] ?? null)}
      <div class="histogram-wrap"></div>
      ${bandTemplate(st, m)} ${compositionTemplate(st, m)} ${flamegraphLinksTemplate(m)}
      ${exemplarsTemplate(exemplars, tableCtx, hasBand, m.exemplarRefreshPending)}
      ${m.coverage
        ? html`<div class="coverage">${formatCoverageBadge(m.coverage)}</div>`
        : nothing}
    `,
    detailPanel,
  );

  const histWrap = detailPanel.querySelector<HTMLElement>(".histogram-wrap");
  if (histWrap) renderHistogram(histWrap, st.histogram, m.band, m.onBand);
}
