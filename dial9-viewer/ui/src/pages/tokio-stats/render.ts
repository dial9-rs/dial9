// The page's DECLARATIVE rendering (the structural XSS guard). Every
// server/URL-derived string (spawn locations from traced code, the exemplar
// deep-link URL) reaches the DOM only through lit-html interpolation, which
// escapes text content and sets attribute values via the DOM API - never
// innerHTML with interpolated data. This is the guard for the #587
// vulnerability: hostile strings are inert by construction (they render as
// text, not markup - see render.test.ts).

import { html, nothing, render, type TemplateResult } from "lit-html";
import type { TokioStatsResponse } from "../../lib/trace/index.js";
import { formatDuration, latencyHeat, nsToDatetime } from "./format.js";
import { exemplarLink } from "./exemplar.js";
import {
  buildDiffModel,
  singlePeriodRows,
  type DiffModel,
  type LocStats,
  type PeriodStats,
} from "./stats.js";

/** Period-tag palette, cycling blue/purple/green/orange. */
export const COLORS = ["#58a6ff", "#d2a8ff", "#7ee787", "#ffa657"];

/** Minimal period shape the row template needs. */
export interface PeriodRow {
  id: number;
  startNs: string | null;
  endNs: string | null;
}

/** Handlers wired to the period-row inline controls. */
export interface PeriodHandlers {
  onUpdate: (id: number, field: "start" | "end", value: string) => void;
  onRemove: (id: number) => void;
}

/** One view tab. */
export interface Tab {
  id: string;
  label: string;
}

function inputValue(e: Event): string {
  return (e.target as HTMLInputElement).value;
}

/** The period-management rows: colored P{i} tag, From/To, remove. */
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

/** Render the period rows into their container. */
export function renderPeriods(
  el: HTMLElement,
  periods: readonly PeriodRow[],
  utc: boolean,
  handlers: PeriodHandlers,
): void {
  render(periodsTemplate(periods, utc, handlers), el);
}

/** The view-tab strip; hidden by the caller until 2+ periods loaded. */
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

/** A class-count cell: an exemplar link when available, else plain. */
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

/** The single-period summary cards. */
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

/**
 * The per-location table, or the empty-threshold state. Exported for the XSS
 * regression test: it is the sink for the attacker-influenceable spawn_loc (the
 * #587 vuln) and the exemplar deep-link URL.
 */
export function locTableTemplate(
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

/**
 * "Longest polls": the single polls that held a worker thread longest — the
 * server ranks them (top_long_polls); the client filters by the same threshold
 * slider as the table above so the two stay consistent. Each row deep-links its
 * trace segment through the shared exemplarLink (LongPoll extends PollExemplar).
 * lit-html escapes every interpolated value, so the attacker-influenceable
 * spawn_loc/host render inert.
 */
export function longPollsTemplate(
  data: TokioStatsResponse,
  threshNs: number,
  bucketParam: string | null,
  onOpen: (url: string) => void,
): TemplateResult | typeof nothing {
  const rows = (data.top_long_polls ?? []).filter((p) => p.duration_ns >= threshNs).slice(0, 50);
  if (!rows.length) return nothing;
  // The host column only earns its space when the scope spans multiple hosts.
  const multiHost = new Set(rows.map((p) => p.host || "")).size > 1;
  const bucketFromData = data.bucket;
  return html`
    <h3 style="margin:20px 0 8px">Longest polls</h3>
    <p style="font-size:0.8em;color:#8b949e;margin-bottom:6px">
      Single polls that held a worker thread longest; long polls starve other
      tasks on that worker. Click a row to open its trace segment.
    </p>
    <table>
      <thead>
        <tr>
          <th>Duration</th>
          <th>Worker</th>
          <th>Task</th>
          <th>Spawn Location</th>
          ${multiHost ? html`<th>Host</th>` : nothing}
        </tr>
      </thead>
      <tbody>
        ${rows.map((p) => {
          const url = exemplarLink(p, bucketFromData, bucketParam);
          const cells = html`
            <td>
              <span style="color:${latencyHeat(p.duration_ns)};font-weight:600"
                >${formatDuration(p.duration_ns)}</span
              >
            </td>
            <td>w${p.worker_id}</td>
            <td><code>${p.task_id}</code></td>
            <td><code>${p.spawn_loc || "(unknown)"}</code></td>
            ${multiHost ? html`<td>${p.host || ""}</td>` : nothing}
          `;
          return url
            ? html`<tr style="cursor:pointer" data-url=${url} @click=${() => onOpen(url)}>
                ${cells}
              </tr>`
            : html`<tr>${cells}</tr>`;
        })}
      </tbody>
    </table>
  `;
}

/** Render one period through the single-period view. */
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
  render(
    html`${locTableTemplate(s, data, bucketParam, onOpen)}${longPollsTemplate(
      data,
      threshNs,
      bucketParam,
      onOpen,
    )}`,
    tableEl,
  );
}

/** Render the "Not loaded yet." guard for a stale detail tab. */
export function renderNotLoaded(tableEl: HTMLElement): void {
  render(html`<p>Not loaded yet.</p>`, tableEl);
}

/** The four diff summary cards. */
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

/** One ranked diff table. Exported for the XSS regression test (the diff tables
 * are the second spawn_loc sink). */
export function diffTableTemplate(
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

/** The diff view: cards + Regressions / New offenders / Improvements. */
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
