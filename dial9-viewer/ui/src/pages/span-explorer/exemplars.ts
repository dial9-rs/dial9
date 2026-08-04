// The exemplar table: representative slow instances of the selected span type.
//
// Columns adapt to whatever per-instance metadata the exemplars carry - a Host
// column, a Composition column when at least one has composition data, and one
// column PER attribute key present across the set. Any non-core column can be
// hidden through the Columns menu, and uniform ones hide themselves.
//
// A per-row Jump button deep-links to the viewer focused on that exact span;
// the row itself is deliberately NOT clickable, so the attribute filter/copy
// actions cannot navigate away by accident.

import { html, nothing, type TemplateResult } from "lit-html";
import {
  collectExemplarAttributeKeys,
  computeTimeComposition,
  exemplarAttrValue,
  exemplarViewerUrl,
  fmtNs,
  fmtPercentile,
  hasAttrFilter,
  percentileForDuration,
} from "../../lib/trace/index.js";
import type {
  AttrFilter,
  Exemplar,
  ExemplarLinkScope,
  HistogramBarLike,
  SpanTypeStats,
} from "../../lib/trace/index.js";
import {
  colVisibility,
  type ColumnOverrides,
  type VisibilityInput,
} from "./columns.js";

/** One exemplar-table column: header, cell, and its visibility inputs. */
interface Column extends VisibilityInput<Exemplar> {
  /** Header content; a template so `%ile` can carry its explanatory title. */
  th: TemplateResult | string;
  /** Menu label; falls back to the id when absent. */
  label?: string;
  cell: (ex: Exemplar, index: number) => TemplateResult;
}

/** Everything the table needs beyond the exemplars themselves. */
export interface ExemplarTableCtx {
  spanType: SpanTypeStats;
  scope: ExemplarLinkScope;
  attrFilters: readonly AttrFilter[];
  overrides: ColumnOverrides;
  /** Raw-trace mode cannot re-query server aggregates for attribute filters. */
  rawMode: boolean;
  onToggleFilter: (key: string, value: string) => void;
  onSetOverride: (id: string, value: boolean | undefined) => void;
  onResetOverrides: () => void;
}

/**
 * A coarse signature of an exemplar's composition (rounded per-category
 * percentages), used ONLY to decide whether the composition column is
 * degenerate. Two instances with effectively the same breakdown share a
 * signature; a missing composition yields "" so an all-missing column collapses.
 */
export function compositionSignature(ex: Exemplar): string {
  if (ex.composition == null) return "";
  const comp = computeTimeComposition({ composition: ex.composition }, null);
  return comp.categories.map((c) => Math.round(c.frac * 100)).join(",");
}

/** Copy text, flashing the button. Falls back to a prompt without clipboard. */
async function copyToClipboard(text: string, btn: HTMLElement): Promise<void> {
  const flash = (): void => {
    const orig = btn.textContent;
    btn.textContent = "✓";
    btn.classList.add("copied");
    setTimeout(() => {
      btn.textContent = orig;
      btn.classList.remove("copied");
    }, 1000);
  };
  try {
    await navigator.clipboard.writeText(text);
    flash();
  } catch {
    window.prompt("Copy:", text);
  }
}

/** One instance's five-way split as a compact stacked bar. */
function compositionCell(ex: Exemplar): TemplateResult {
  if (ex.composition == null) return html`<span class="muted">—</span>`;
  const comp = computeTimeComposition({ composition: ex.composition }, null);
  const shown = comp.categories.filter((c) => c.frac > 0);
  const tip = shown
    .map((c) => `${c.label} ${fmtNs(c.ns)} (${(c.frac * 100).toFixed(0)}%)`)
    .join(" · ");
  return html`<span class="ex-comp-bar" title=${tip}
    >${shown.map(
      (c) => html`<div
        class="seg"
        style="width:${(c.frac * 100).toFixed(1)}%;background:${c.color}"
        title="${c.label}: ${fmtNs(c.ns)} (${(c.frac * 100).toFixed(1)}%)"
      >
        <!-- The time only fits inside sufficiently wide segments; narrower
             ones rely on the tooltip. -->
        ${c.frac >= 0.2 && c.ns > 0
          ? html`<span class="seg-label">${fmtNs(c.ns)}</span>`
          : nothing}
      </div>`,
    )}</span
  >`;
}

/** One attribute value: click to toggle a whole-view filter, plus a copy button. */
function attrCell(ex: Exemplar, key: string, ctx: ExemplarTableCtx): TemplateResult {
  const value = exemplarAttrValue(ex, key);
  if (value == null) return html`<td class="attr-cell"><span class="muted">—</span></td>`;
  const active = hasAttrFilter(ctx.attrFilters, key, value);
  const title = ctx.rawMode
    ? "Attribute filtering requires the aggregation backend"
    : active
      ? "Remove this filter"
      : `Filter to ${key}=${value}`;
  const classes = `attr-val${active ? " active" : ""}${ctx.rawMode ? " inert" : ""}`;
  return html`<td class="attr-cell">
    <span
      class=${classes}
      title=${title}
      @click=${() => {
        if (!ctx.rawMode) ctx.onToggleFilter(key, value);
      }}
      >${value}</span
    ><button
      class="attr-copy"
      title="Copy value"
      @click=${(e: Event) => void copyToClipboard(value, e.currentTarget as HTMLElement)}
    >
      ⧉
    </button>
  </td>`;
}

/** Build the column set for one exemplar list. */
function buildColumns(exemplars: readonly Exemplar[], ctx: ExemplarTableCtx): Column[] {
  const histogram: readonly HistogramBarLike[] = ctx.spanType.histogram ?? [];
  // Jump and the row index are core navigation and never hide.
  const columns: Column[] = [
    {
      id: "jump",
      th: "",
      hideable: false,
      cell: (ex) => {
        const url = exemplarViewerUrl(ex, ctx.scope);
        return html`<td class="jump-cell">
          ${url
            ? html`<button
                class="jump-btn"
                title="Open the viewer focused on this span instance"
                @click=${() => window.open(url, "_blank")}
              >
                Jump ↗
              </button>`
            : html`<button class="jump-btn" disabled title="No source available">
                Jump ↗
              </button>`}
        </td>`;
      },
    },
    {
      id: "index",
      th: "#",
      hideable: false,
      cell: (_ex, i) => html`<td class="muted">${i + 1}</td>`,
    },
    // Duration and %ile carry no degenValue: they stay informative even when the
    // bounded exemplars are close in value, and are never all-empty.
    {
      id: "duration",
      th: "Duration",
      label: "Duration",
      cell: (ex) => html`<td class="dur">${fmtNs(ex.elapsed_ns)}</td>`,
    },
    {
      id: "pctile",
      th: html`<span
        title="Where this instance falls in the span type's duration distribution"
        >%ile</span
      >`,
      label: "%ile",
      cell: (ex) => {
        const pctile = percentileForDuration(histogram, ex.elapsed_ns);
        return html`<td
          class="pctile"
          title=${pctile == null ? "" : `approx ${pctile.toFixed(2)}th percentile`}
        >
          ${fmtPercentile(pctile)}
        </td>`;
      },
    },
    {
      id: "callsite",
      th: "Callsite",
      label: "Callsite",
      degenValue: (ex) =>
        ex.callsite_file != null ? `${ex.callsite_file}:${ex.callsite_line ?? ""}` : "",
      cell: (ex) =>
        ex.callsite_file != null
          ? html`<td>
              <code
                >${ex.callsite_file.split("/").pop()}${ex.callsite_line != null
                  ? `:${ex.callsite_line}`
                  : ""}</code
              >
            </td>`
          : html`<td><code><span class="muted">—</span></code></td>`,
    },
    {
      id: "host",
      th: "Host",
      label: "Host",
      degenValue: (ex) => ex.host || "",
      cell: (ex) => html`<td class="host">${ex.host || ""}</td>`,
    },
  ];

  if (exemplars.some((e) => e.composition != null)) {
    columns.push({
      id: "composition",
      th: "Time composition",
      label: "Time composition",
      degenValue: compositionSignature,
      cell: (ex) => html`<td>${compositionCell(ex)}</td>`,
    });
  }

  // Attribute columns share one id namespace so an override for a key survives
  // renders where the visible attribute set changes.
  for (const key of collectExemplarAttributeKeys(exemplars)) {
    columns.push({
      id: `attr:${key}`,
      th: key,
      label: key,
      degenValue: (ex) => exemplarAttrValue(ex, key),
      cell: (ex) => attrCell(ex, key, ctx),
    });
  }
  return columns;
}

/** The "Columns ▾" show/hide menu. */
function columnMenu(
  columns: readonly Column[],
  exemplars: readonly Exemplar[],
  ctx: ExemplarTableCtx,
): TemplateResult {
  const hideable = columns.filter((c) => c.hideable !== false);
  const states = hideable.map((c) => ({ col: c, vis: colVisibility(c, exemplars, ctx.overrides) }));
  const hiddenCount = states.filter((s) => s.vis.hidden).length;
  const hasOverrides = hideable.some((c) => ctx.overrides[c.id] !== undefined);

  const toggleMenu = (e: Event): void => {
    const box = (e.currentTarget as HTMLElement).parentElement;
    const menu = box?.querySelector<HTMLElement>(".col-menu-list");
    if (!box || !menu) return;
    e.stopPropagation();
    const open = menu.style.display !== "none";
    menu.style.display = open ? "none" : "block";
    if (open) return;
    const onDoc = (ev: MouseEvent): void => {
      if (!box.contains(ev.target as Node)) {
        menu.style.display = "none";
        document.removeEventListener("click", onDoc);
      }
    };
    setTimeout(() => document.addEventListener("click", onDoc), 0);
  };

  return html`<div class="col-menu">
    <button type="button" class="col-menu-btn" @click=${toggleMenu}>
      ${hiddenCount > 0 ? `Columns (${hiddenCount} hidden) ▾` : "Columns ▾"}
    </button>
    <div class="col-menu-list" style="display:none">
      ${states.map(
        ({ col, vis }) => html`<label class="col-menu-item">
          <input
            type="checkbox"
            .checked=${!vis.hidden}
            @change=${(e: Event) =>
              ctx.onSetOverride(col.id, (e.target as HTMLInputElement).checked)}
          />
          ${col.label ?? col.id}
          ${vis.auto ? html`<span class="col-menu-auto"> (auto-hidden)</span>` : nothing}
        </label>`,
      )}
      ${hasOverrides
        ? html`<button type="button" class="col-menu-reset" @click=${ctx.onResetOverrides}>
            Reset to auto
          </button>`
        : nothing}
    </div>
  </div>`;
}

/**
 * The exemplar section. `exemplars` is already band-filtered by the caller.
 * `pending` renders the refreshing notice instead of an empty-set message.
 */
export function exemplarsTemplate(
  exemplars: readonly Exemplar[],
  ctx: ExemplarTableCtx,
  hasBand: boolean,
  pending: boolean,
): TemplateResult | typeof nothing {
  if (exemplars.length === 0 && !hasBand) return nothing;
  const label = hasBand ? "Exemplars in selected band:" : "Exemplars:";

  if (exemplars.length === 0) {
    return html`<div class="exemplars">
      <div class="label">${label}</div>
      <div class="muted">
        ${pending
          ? "Refreshing matching exemplars…"
          : "No matching exemplars are available for this exact duration band."}
      </div>
    </div>`;
  }

  const columns = buildColumns(exemplars, ctx);
  const visible = columns.filter((c) => !colVisibility(c, exemplars, ctx.overrides).hidden);
  return html`<div class="exemplars">
    <div class="label">${label}</div>
    ${columnMenu(columns, exemplars, ctx)}
    <table class="exemplar-table">
      <thead>
        <tr>
          ${visible.map((c) => html`<th>${c.th}</th>`)}
        </tr>
      </thead>
      <tbody>
        ${exemplars.map(
          (ex, i) => html`<tr>
            ${visible.map((c) => c.cell(ex, i))}
          </tr>`,
        )}
      </tbody>
    </table>
  </div>`;
}
