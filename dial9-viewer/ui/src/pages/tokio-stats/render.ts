// src/pages/tokio-stats/render.ts - the page's DECLARATIVE rendering (N17,
// the structural XSS guard). Every server/URL-derived string (spawn
// locations from traced code, the exemplar deep-link URL) reaches the DOM
// only through lit-html interpolation, which escapes text content and sets
// attribute values via the DOM API - never innerHTML with interpolated data.
//
// This is the guard for the #587 vulnerability (features/04 I1): the legacy
// page hand-escaped spawn_loc with escapeHtml; here hostile strings are inert
// by construction (they render as text, not markup - see render.test.ts).
//
// The templates mirror the legacy DOM structure/classes/text exactly so the
// affordance census is zero-diff (tokio_stats.html:116-369). The only
// intentional divergence from the legacy markup is the exemplar count link's
// event wiring: an @click handler replaces the inline onclick + global
// openExemplar, keeping the same href="#" + class + data-url attributes.

import { html, nothing, render, type TemplateResult } from "lit-html";
import type { TokioStatsResponse } from "../../lib/trace/index.js";
import { formatDuration, nsToDatetime } from "./format.js";
import { exemplarLink } from "./exemplar.js";
import {
  buildDiffModel,
  singlePeriodRows,
  type DiffModel,
  type LocStats,
  type PeriodStats,
} from "./stats.js";

/** Period-tag palette, cycling blue/purple/green/orange (B1). */
export const COLORS = ["#58a6ff", "#d2a8ff", "#7ee787", "#ffa657"];

/** Minimal period shape the row template needs. */
export interface PeriodRow {
  id: number;
  startNs: string | null;
  endNs: string | null;
}

/** Handlers wired to the period-row inline controls (B2/B5/B6). */
export interface PeriodHandlers {
  onUpdate: (id: number, field: "start" | "end", value: string) => void;
  onRemove: (id: number) => void;
}

/** One view tab (G1). */
export interface Tab {
  id: string;
  label: string;
}

function inputValue(e: Event): string {
  return (e.target as HTMLInputElement).value;
}

/** The period-management rows (B1): colored P{i} tag, From/To, remove. */
function periodsTemplate(
  periods: readonly PeriodRow[],
  utc: boolean,
  handlers: PeriodHandlers,
): TemplateResult {
  return html`${periods.map((p, i) => {
    const color = COLORS[i % COLORS.length];
    return html`
      <div class="period-row">
        <span class="tag" style="background:${color}33;color:${color}">P${i + 1}</span>
        <label
          >From:
          <input
            type="datetime-local"
            step="1"
            .value=${nsToDatetime(p.startNs, utc)}
            @change=${(e: Event) => handlers.onUpdate(p.id, "start", inputValue(e))}
        /></label>
        <label
          >To:
          <input
            type="datetime-local"
            step="1"
            .value=${nsToDatetime(p.endNs, utc)}
            @change=${(e: Event) => handlers.onUpdate(p.id, "end", inputValue(e))}
        /></label>
        ${periods.length > 1
          ? html`<button
              class="secondary"
              @click=${() => handlers.onRemove(p.id)}
              style="padding:2px 8px;font-size:0.8em"
            >
              ✕
            </button>`
          : nothing}
      </div>
    `;
  })}`;
}

/** Render the period rows into their container (B1). */
export function renderPeriods(
  el: HTMLElement,
  periods: readonly PeriodRow[],
  utc: boolean,
  handlers: PeriodHandlers,
): void {
  render(periodsTemplate(periods, utc, handlers), el);
}

/** The view-tab strip (G1); hidden by the caller until 2+ periods loaded. */
export function renderTabs(
  el: HTMLElement,
  tabs: readonly Tab[],
  activeTab: string,
  onSetTab: (id: string) => void,
): void {
  render(
    html`${tabs.map(
      (t) => html`<span
        class="view-tab ${t.id === activeTab ? "active" : ""}"
        @click=${() => onSetTab(t.id)}
        >${t.label}</span
      >`,
    )}`,
    el,
  );
}

/** A class-count cell: an exemplar link when available, else plain (H2/H3). */
function linkedStat(
  count: number,
  exemplar: LocStats["exemplars"][number],
  bucketFromData: string | null,
  bucketParam: string | null,
  cls: string,
  onOpen: (url: string) => void,
): TemplateResult | typeof nothing {
  if (!count) return nothing;
  const url = exemplarLink(exemplar, bucketFromData, bucketParam);
  if (url) {
    return html`<a
      href="#"
      class=${cls}
      data-url=${url}
      @click=${(e: Event) => {
        e.preventDefault();
        onOpen(url);
      }}
      >${count}</a
    >`;
  }
  return html`<span class=${cls}>${count}</span>`;
}

/** The single-period summary cards (F1). */
function summaryCardsTemplate(s: PeriodStats, threshNs: number): TemplateResult {
  return html`
    <div class="card">
      <div class="value">${s.total_polls.toLocaleString()}</div>
      <div class="label">Total Polls</div>
    </div>
    <div class="card ${s.totalLong > 0 ? "warn" : "good"}">
      <div class="value">${s.totalLong.toLocaleString()}</div>
      <div class="label">Long (>${formatDuration(threshNs)})</div>
    </div>
    <div class="card ${s.totalLong > 0 ? "warn" : "good"}">
      <div class="value">${s.rate.toFixed(1)}/min</div>
      <div class="label">Rate</div>
    </div>
    <div class="card ${s.totalOffCpu > 0 ? "bad" : "good"}">
      <div class="value">${s.totalOffCpu.toLocaleString()}</div>
      <div class="label">Off-CPU (blocked)</div>
    </div>
    <div class="card on-cpu">
      <div class="value">${s.totalOnCpu.toLocaleString()}</div>
      <div class="label">On-CPU (compute)</div>
    </div>
  `;
}

/** The per-location table (F2), or the empty-threshold state (F3). */
function locTableTemplate(
  s: PeriodStats,
  data: TokioStatsResponse,
  bucketParam: string | null,
  onOpen: (url: string) => void,
): TemplateResult {
  const rows = singlePeriodRows(s);
  if (!rows.length) {
    return html`<p style="margin-top:10px">No polls exceed the threshold.</p>`;
  }
  const bucketFromData = data.bucket;
  return html`
    <table>
      <thead>
        <tr>
          <th>Spawn Location</th>
          <th>Long</th>
          <th>/min</th>
          <th>Off-CPU 🔴</th>
          <th>On-CPU 🟡</th>
          <th>P50</th>
          <th>P99</th>
          <th>Max</th>
        </tr>
      </thead>
      <tbody>
        ${rows.map(
          ({ loc, v }) => html`<tr>
            <td><code>${loc}</code></td>
            <td>${v.long}</td>
            <td>${v.rate.toFixed(1)}</td>
            <td>
              ${linkedStat(
                v.offCpu,
                v.exemplars && v.exemplars[0],
                bucketFromData,
                bucketParam,
                "off-cpu",
                onOpen,
              )}
            </td>
            <td>
              ${linkedStat(
                v.onCpu,
                v.exemplars && v.exemplars[1],
                bucketFromData,
                bucketParam,
                "on-cpu",
                onOpen,
              )}
            </td>
            <td>${formatDuration(v.p50)}</td>
            <td>${formatDuration(v.p99)}</td>
            <td>${formatDuration(v.max)}</td>
          </tr>`,
        )}
      </tbody>
    </table>
  `;
}

/** Render one period through the single-period view (F1/F2/F3). */
export function renderSinglePeriod(
  sumEl: HTMLElement,
  tableEl: HTMLElement,
  s: PeriodStats,
  data: TokioStatsResponse,
  threshNs: number,
  bucketParam: string | null,
  onOpen: (url: string) => void,
): void {
  render(summaryCardsTemplate(s, threshNs), sumEl);
  render(locTableTemplate(s, data, bucketParam, onOpen), tableEl);
}

/** Render the "Not loaded yet." guard for a stale detail tab (G2). */
export function renderNotLoaded(tableEl: HTMLElement): void {
  render(html`<p>Not loaded yet.</p>`, tableEl);
}

/** The four diff summary cards (G4). */
function diffCardsTemplate(
  model: DiffModel,
  periodsLength: number,
): TemplateResult {
  const c = model.cards;
  return html`
    <div class="card ${c.cls}">
      <div class="value">
        ${c.firstRate.toFixed(1)} ${c.arrow} ${c.lastRate.toFixed(1)}/min
      </div>
      <div class="label">Long poll rate (${c.ratePctLabel})</div>
    </div>
    <div class="card ${c.offCls}">
      <div class="value">${c.offFirst.toFixed(1)} → ${c.offLast.toFixed(1)}/min</div>
      <div class="label">Off-CPU rate</div>
    </div>
    <div class="card">
      <div class="value">${c.firstTotalPolls.toLocaleString()}</div>
      <div class="label">P1 polls</div>
    </div>
    <div class="card">
      <div class="value">${c.lastTotalPolls.toLocaleString()}</div>
      <div class="label">P${periodsLength} polls</div>
    </div>
  `;
}

/** One ranked diff table (G9). */
function diffTableTemplate(
  rows: DiffModel["regressions"],
  cls: string,
  periodsLength: number,
): TemplateResult {
  return html`
    <table>
      <thead>
        <tr>
          <th>Spawn Location</th>
          <th>P1 /min</th>
          <th>P${periodsLength} /min</th>
          <th>Δ /min</th>
          <th>Δ%</th>
        </tr>
      </thead>
      <tbody>
        ${rows.map(
          (d) => html`<tr>
            <td><code>${d.loc}</code></td>
            <td>${d.fRate.toFixed(1)}</td>
            <td>${d.lRate.toFixed(1)}</td>
            <td class=${cls}>${d.delta > 0 ? "+" : ""}${d.delta.toFixed(1)}</td>
            <td class=${cls}>${d.pct}</td>
          </tr>`,
        )}
      </tbody>
    </table>
  `;
}

/** The diff view: cards + Regressions / New offenders / Improvements (G4-G8). */
export function renderDiff(
  sumEl: HTMLElement,
  tableEl: HTMLElement,
  model: DiffModel,
  periodsLength: number,
): void {
  render(diffCardsTemplate(model, periodsLength), sumEl);

  const sections: TemplateResult[] = [];
  if (model.regressions.length) {
    sections.push(html`<h3 style="margin:16px 0 8px;color:#f85149">
        ⬆ Regressions (${model.regressions.length})
      </h3>
      ${diffTableTemplate(model.regressions, "delta-bad", periodsLength)}`);
  }
  if (model.newOffenders.length) {
    sections.push(html`<h3 style="margin:16px 0 8px;color:#f85149">
        🆕 New offenders (${model.newOffenders.length})
      </h3>
      ${diffTableTemplate(model.newOffenders, "delta-bad", periodsLength)}`);
  }
  if (model.improvements.length) {
    sections.push(html`<h3 style="margin:16px 0 8px;color:#3fb950">
        ⬇ Improvements (${model.improvements.length})
      </h3>
      ${diffTableTemplate(model.improvements, "delta-good", periodsLength)}`);
  }
  if (!sections.length) {
    render(
      html`<p style="margin-top:10px">No significant changes between periods.</p>`,
      tableEl,
    );
    return;
  }
  render(html`${sections}`, tableEl);
}

// Re-export so main.ts builds the diff model through one import surface.
export { buildDiffModel };
