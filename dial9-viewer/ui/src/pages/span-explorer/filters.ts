// The active attribute-filter bar. Filters narrow the whole aggregate
// server-side (counts, histograms, composition, exemplars), so toggling one
// re-runs the catalog stream rather than filtering client-side.

import { html, render } from "lit-html";
import type { AttrFilter } from "../../lib/trace/index.js";

/** Render the bar, hiding it entirely when no filter is active. */
export function renderFilterBar(
  bar: HTMLElement,
  filters: readonly AttrFilter[],
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
      <span class="fb-label">Attribute filters:</span>
      ${filters.map(
        (f) => html`<span class="fb-chip"
          ><b>${f.key}</b>=${f.value}
          <span class="fb-x" title="Remove" @click=${() => onRemove(f.key, f.value)}>×</span></span
        >`,
      )}
      <span class="fb-clear" @click=${onClearAll}>clear all</span>
    `,
    bar,
  );
}
