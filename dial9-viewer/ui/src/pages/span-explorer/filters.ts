// The active histogram-comparison bar. Attribute constraints apply only to the
// selected span type's second histogram; the catalog and baseline remain intact.

import { html, render } from "lit-html";
import type { AttrFilter } from "../../lib/trace/index.js";

/** Human-readable AND expression used by the bar and histogram label. */
export function formatAttrComparisonLabel(filters: readonly AttrFilter[]): string {
  return filters.map(({ key, value }) => `${key}=${value}`).join(" AND ");
}

/** Render the bar, hiding it entirely when no comparison is active. */
export function renderFilterBar(
  bar: HTMLElement,
  filters: readonly AttrFilter[],
  pending: boolean,
  error: string | null,
  onRemove: (key: string, value: string) => void,
  onClearAll: () => void,
): void {
  if (filters.length === 0) {
    bar.style.display = "none";
    render(html``, bar);
    return;
  }
  bar.style.display = "flex";
  render(
    html`
      <span class="fb-label">Histogram comparison:</span>
      ${filters.map(
        (f) => html`<span class="fb-chip"
          ><b>${f.key}</b>=${f.value}
          <span class="fb-x" title="Remove" @click=${() => onRemove(f.key, f.value)}>×</span></span
        >`,
      )}
      <span class="fb-note">filter vs. Not(filter)</span>
      ${pending
        ? html`<span class="fb-status"><span class="spinner"></span> updating…</span>`
        : error
          ? html`<span class="fb-status error-text">${error}</span>`
          : ""}
      <span class="fb-clear" @click=${onClearAll}>clear all</span>
    `,
    bar,
  );
}
