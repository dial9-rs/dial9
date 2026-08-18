// The page's DECLARATIVE rendering (the structural XSS guard). Every
// server/URL-derived string (spawn locations from traced code, the exemplar
// deep-link URL) reaches the DOM only through lit-html interpolation, which
// escapes text content and sets attribute values via the DOM API - never
// innerHTML with interpolated data. This is the guard for the #587
// vulnerability: hostile strings are inert by construction (they render as
// text, not markup - see render.test.ts).

import { html, nothing, render, type TemplateResult } from "lit-html";
import { live } from "lit-html/directives/live.js";
import type { SchedulingDelay, TokioStatsResponse } from "../../lib/trace/index.js";
import { formatDuration, nsToDatetime, schedulingDelayEvidenceLabel } from "./format.js";
import { busynessHeat, latencyHeat } from "../../lib/trace/tokio_stats_api.js";
import { exemplarLink } from "./exemplar.js";
import {
  buildDiffModel,
  buildHostRows,
  singlePeriodRows,
  sortedWorkers,
  type DiffModel,
  type HostRow,
  type LocStats,
  type PeriodStats,
  type WorkerSortKey,
} from "./stats.js";

/** Period-tag palette, cycling blue/purple/green/orange. */
export const COLORS = ["#58a6ff", "#d2a8ff", "#7ee787", "#ffa657"];

/** Row-count choices offered by every per-table "Rows" selector. */
const ROW_LIMIT_OPTIONS = [10, 25, 50, 100] as const;

/**
 * A per-table "Rows: [n]" selector. `live()` keeps the shown option pinned to
 * the caller's current limit across re-renders (lit-html otherwise leaves a
 * user-edited <select> untouched). onChange receives the parsed count.
 */
function rowLimitSelect(
  current: number,
  onChange: (limit: number) => void,
): TemplateResult {
  return html`<label style="font-size:0.8em;font-weight:400;color:#8b949e">
    Rows:
    <select
      .value=${live(String(current))}
      @change=${(e: Event) => onChange(Number((e.target as HTMLSelectElement).value))}
      style="background:#1a1a2e;color:#e0e0e0;border:1px solid #444;padding:2px 4px;border-radius:3px"
    >
      ${ROW_LIMIT_OPTIONS.map((n) => html`<option value=${n}>${n}</option>`)}
    </select>
  </label>`;
}

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
  limit: number,
  onLimitChange: (limit: number) => void,
): TemplateResult | typeof nothing {
  const matching = (data.top_long_polls ?? []).filter(
    (p) => p.duration_ns >= threshNs,
  );
  if (!matching.length) return nothing;
  const rows = matching.slice(0, limit);
  // The host column only earns its space when the scope spans multiple hosts.
  const multiHost = new Set(rows.map((p) => p.host || "")).size > 1;
  const bucketFromData = data.bucket;
  return html`
    <h3 style="margin:20px 0 8px;display:flex;align-items:center;gap:12px">
      🕒 Longest polls ${rowLimitSelect(limit, onLimitChange)}
    </h3>
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

/**
 * "Scheduling delay": how long a runnable task waited before its next poll,
 * backed by same-segment spawn/wake evidence (an inter-poll gap is deliberately
 * never treated as a delay — it can contain legitimate I/O or timer waits). The
 * server ranks the top delays and reports coverage (measured vs why-unmeasured)
 * so the top-N reads against an honest denominator. Each row deep-links its
 * trace segment via the ready -> poll-end focus window. lit-html escapes every
 * interpolated value, so the attacker-influenceable spawn_loc/host render inert.
 */
export function schedulingDelaysTemplate(
  data: TokioStatsResponse,
  bucketParam: string | null,
  onOpen: (url: string) => void,
  limit: number,
  onLimitChange: (limit: number) => void,
): TemplateResult | typeof nothing {
  const cov = data.scheduling_delay_coverage;
  if (!cov) return nothing;

  const observed = cov.observed_polls;
  const unmeasured = cov.unmeasured_polls;
  const total = observed + unmeasured;
  const pct = total > 0 ? ((observed / total) * 100).toFixed(1) : "0.0";
  const rows = (data.top_scheduling_delays ?? []).slice(0, limit);
  const bucketFromData = data.bucket;
  // The host column only earns its space when the scope spans multiple hosts.
  const multiHost = new Set(rows.map((d) => d.host || "")).size > 1;

  const coverageLine = html`<p style="font-size:0.8em;color:#8b949e;margin-bottom:6px">
    ${observed.toLocaleString()} / ${total.toLocaleString()} polls observed (${pct}%) ·
    ${cov.spawn_inferred_polls.toLocaleString()} spawn-inferred ·
    ${cov.wake_observed_polls.toLocaleString()} idle wakes ·
    ${cov.wake_during_poll_polls.toLocaleString()} in-poll wakes ·
    ${cov.over_1ms_polls.toLocaleString()} over 1ms
  </p>`;
  const unmeasuredLine =
    unmeasured > 0
      ? html`<p style="font-size:0.8em;color:#d29922;margin-bottom:6px">
          ${unmeasured.toLocaleString()} polls unmeasured:
          ${cov.uninstrumented_unmeasured_polls.toLocaleString()} uninstrumented,
          ${cov.instrumentation_unknown_unmeasured_polls.toLocaleString()} instrumentation
          unknown, ${cov.missing_readiness_unmeasured_polls.toLocaleString()} missing
          same-segment readiness evidence. These are unknown, not zero-delay.
        </p>`
      : nothing;

  const header = html`
    <h3 style="margin:20px 0 8px;display:flex;align-items:center;gap:12px">
      Longest observed scheduling delays ${rowLimitSelect(limit, onLimitChange)}
    </h3>
    <p style="font-size:0.8em;color:#8b949e;margin-bottom:4px">
      Time a runnable task waited before its next poll. First polls can be
      inferred from task spawn; later polls require a wake in the same trace
      segment. Click a row to open its trace segment.
    </p>
    ${coverageLine}${unmeasuredLine}
  `;

  if (!rows.length) {
    return html`${header}<p style="font-size:0.85em;margin-top:8px">
        No scheduling delay could be safely measured in the folded files.
      </p>`;
  }

  return html`
    ${header}
    <table>
      <thead>
        <tr>
          <th>Delay</th>
          <th>Evidence</th>
          <th>Worker</th>
          <th>Task</th>
          <th>Spawn Location</th>
          ${multiHost ? html`<th>Host</th>` : nothing}
        </tr>
      </thead>
      <tbody>
        ${rows.map((d: SchedulingDelay) => {
          // The exemplar focus window frames the wait: ready -> poll end.
          const url = exemplarLink(
            {
              start_ns: d.ready_at_ns,
              end_ns: d.poll_end_ns,
              duration_ns: d.delay_ns,
              host: d.host,
              source_key: d.source_key,
              worker_id: d.worker_id,
              task_id: d.task_id,
            },
            bucketFromData,
            bucketParam,
          );
          const cells = html`
            <td>
              <span style="color:${latencyHeat(d.delay_ns)};font-weight:600"
                >${formatDuration(d.delay_ns)}</span
              >
            </td>
            <td>${schedulingDelayEvidenceLabel(d.kind)}</td>
            <td>w${d.worker_id}</td>
            <td><code>${d.task_id}</code></td>
            <td><code>${d.spawn_loc || "(unknown)"}</code></td>
            ${multiHost ? html`<td>${d.host || ""}</td>` : nothing}
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

/** The "Worker activity" interaction state and its handlers. */
export interface WorkerActivityState {
  sortKey: WorkerSortKey;
  sortDesc: boolean;
  /** Hosts whose per-worker detail rows are expanded. */
  expandedHosts: ReadonlySet<string>;
  onSort: (key: WorkerSortKey) => void;
  onToggleHost: (host: string) => void;
}

/** A busyness bar; width is clamped so a >100% value cannot overflow the cell. */
function busynessBar(busyPct: number, color: string): TemplateResult {
  const width = Math.min(busyPct, 100);
  return html`<div
    style="background:${color}33;border-radius:2px;height:14px;width:${width}%"
  ></div>`;
}

/** The worst-poll cell: heat-colored duration, or an em dash when there is none. */
function worstPollCell(worstPollNs: number): TemplateResult {
  if (worstPollNs <= 0) return html`—`;
  return html`<span style="color:${latencyHeat(worstPollNs)};font-weight:600"
    >${formatDuration(worstPollNs)}</span
  >`;
}

/** The per-worker detail rows shown under an expanded host. */
function workerDetailRows(
  row: HostRow,
  data: TokioStatsResponse,
  bucketParam: string | null,
  onOpen: (url: string) => void,
): TemplateResult {
  return html`${sortedWorkers(row).map((w) => {
    const url = w.worst_exemplar ? exemplarLink(w.worst_exemplar, data.bucket, bucketParam) : "";
    const busyFmt = `${w.busy_pct.toFixed(3)}%`;
    const color = busynessHeat(w.busy_pct);
    // The verification breakdown the rollup promises: busy / observed = pct.
    const spanFmt = w.span_ns ? formatDuration(w.span_ns) : "?";
    const verify = `${formatDuration(w.busy_ns)} busy / ${spanFmt} span = ${busyFmt}`;
    const cells = html`
      <td style="padding-left:28px;font-weight:normal">w${w.worker_id}</td>
      <td></td>
      <td title=${verify}><span style="color:${color}">${busyFmt}</span></td>
      <td style="width:120px">${busynessBar(w.busy_pct, color)}</td>
      <td>${w.total_polls.toLocaleString()}</td>
      <td>${w.notable_polls.toLocaleString()}</td>
      <td>${worstPollCell(w.worst_poll_ns)}</td>
    `;
    return url
      ? html`<tr
          style="cursor:pointer;background:#1a1f26"
          data-url=${url}
          @click=${() => onOpen(url)}
        >
          ${cells}
        </tr>`
      : html`<tr style="background:#1a1f26">${cells}</tr>`;
  })}`;
}

/**
 * "Worker activity": per-host busyness with expandable per-worker detail. A
 * host's busyness POOLS its workers (Σ busy / Σ observed, via the frozen
 * hostBusyPct) so it stays bounded <=100% and weights each worker by observed
 * time. Exported for the XSS regression test: the host name is
 * attacker-influenceable, but lit-html binds a listener rather than building
 * an inline handler, so a quote in a host name is structurally inert.
 */
export function workerActivityTemplate(
  data: TokioStatsResponse,
  bucketParam: string | null,
  onOpen: (url: string) => void,
  state: WorkerActivityState,
): TemplateResult | typeof nothing {
  const rows = buildHostRows(data, state.sortKey, state.sortDesc);
  if (!rows.length) return nothing;

  const totalWorkers = (data.worker_activity ?? []).length;
  const indicator = (key: WorkerSortKey) =>
    state.sortKey === key ? (state.sortDesc ? " ▼" : " ▲") : "";
  const sortableTh = (label: unknown, key: WorkerSortKey) => html`<th
    style="cursor:pointer"
    @click=${() => state.onSort(key)}
  >
    ${label}${indicator(key)}
  </th>`;

  return html`
    <h3 style="margin:20px 0 8px">⚙️ Worker activity</h3>
    <p style="font-size:0.8em;color:#8b949e;margin-bottom:6px">
      ${totalWorkers} workers across ${rows.length}
      host${rows.length > 1 ? "s" : ""} — busyness = poll time / observed active
      time; a host's busyness pools its workers (Σ busy / Σ observed). Click a
      host row to expand individual workers.
    </p>
    <table>
      <thead>
        <tr>
          ${sortableTh("Host", "host")}
          ${sortableTh(
            html`Workers<br /><span style="font-weight:normal;color:#8b949e"
                >active / total</span
              >`,
            "numWorkers",
          )}
          ${sortableTh("Busy", "busyPct")}
          <th>Bar</th>
          ${sortableTh("Polls", "totalPolls")}
          ${sortableTh("Notable", "notablePolls")}
          ${sortableTh("Worst poll", "worstPollNs")}
        </tr>
      </thead>
      <tbody>
        ${rows.map((row) => {
          const expanded = state.expandedHosts.has(row.host);
          const busyFmt = `${row.busyPct.toFixed(3)}%`;
          const color = busynessHeat(row.busyPct);
          return html`
            <tr
              style="cursor:pointer;font-weight:600"
              @click=${() => state.onToggleHost(row.host)}
            >
              <td>${expanded ? "▾" : "▸"} ${row.host}</td>
              <td>${row.activeWorkers} / ${row.numWorkers}</td>
              <td><span style="color:${color}">${busyFmt}</span></td>
              <td style="width:120px">${busynessBar(row.busyPct, color)}</td>
              <td>${row.totalPolls.toLocaleString()}</td>
              <td>${row.notablePolls.toLocaleString()}</td>
              <td>${worstPollCell(row.worstPollNs)}</td>
            </tr>
            ${expanded ? workerDetailRows(row, data, bucketParam, onOpen) : nothing}
          `;
        })}
      </tbody>
    </table>
  `;
}

/** Per-table row limits and their change handlers for the single-period view. */
export interface RowLimits {
  longPolls: number;
  onLongPollsChange: (limit: number) => void;
  schedulingDelays: number;
  onSchedulingDelaysChange: (limit: number) => void;
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
  limits: RowLimits,
  workers: WorkerActivityState,
): void {
  render(summaryCardsTemplate(s, threshNs), sumEl);
  render(
    html`${locTableTemplate(s, data, bucketParam, onOpen)}${workerActivityTemplate(
      data,
      bucketParam,
      onOpen,
      workers,
    )}${schedulingDelaysTemplate(
      data,
      bucketParam,
      onOpen,
      limits.schedulingDelays,
      limits.onSchedulingDelaysChange,
    )}${longPollsTemplate(
      data,
      threshNs,
      bucketParam,
      onOpen,
      limits.longPolls,
      limits.onLongPollsChange,
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
