// The span-type catalog table: one row per span type, sortable by any column.
//
// Declarative rendering is the structural XSS guard - span names, targets and
// callsites come from traced user code, and reach the DOM only through
// lit-html interpolation (escaped text / DOM-set attributes), never innerHTML.

import { html, render, type TemplateResult } from "lit-html";
import { fmtNs, sortSpanTypes, spanTypeLabel, spanTypeQuality } from "../../lib/trace/index.js";
import type { SpanTypeStats } from "../../lib/trace/index.js";

/** Sortable catalog columns; `quality` is derived, the rest are wire fields. */
export type SortKey =
  | "name"
  | "kind"
  | "count"
  | "p50_ns"
  | "p95_ns"
  | "p99_ns"
  | "max_ns"
  | "quality";

export interface CatalogSort {
  key: SortKey;
  ascending: boolean;
}

/** A span type carrying its derived quality, so `quality` is sortable. */
type Sortable = SpanTypeStats & { quality?: number };

/**
 * Sort for display. `quality` is attached to a COPY - the wire objects the
 * stream owns are never mutated.
 */
export function sortedForDisplay(
  spanTypes: readonly SpanTypeStats[],
  sort: CatalogSort,
): SpanTypeStats[] {
  const augmented: Sortable[] = spanTypes.map((st) => {
    const q = spanTypeQuality(st);
    return q != null ? { ...st, quality: q } : st;
  });
  return sortSpanTypes(augmented, sort.key, sort.ascending);
}

/** Next sort state for a header click: same column flips, a new column resets. */
export function nextSort(current: CatalogSort, key: SortKey): CatalogSort {
  return current.key === key
    ? { key, ascending: !current.ascending }
    : { key, ascending: false };
}

function rowTemplate(
  st: SpanTypeStats,
  selectedUid: string | null,
  onSelect: (uid: string) => void,
): TemplateResult {
  const label = spanTypeLabel(st);
  const quality = spanTypeQuality(st);
  const kindClass = st.kind === "tracing" ? "kind-tracing" : "kind-tokio_poll";
  return html`
    <tr
      class=${st.span_type_uid === selectedUid ? "selected" : ""}
      data-uid=${st.span_type_uid}
      @click=${() => onSelect(st.span_type_uid)}
    >
      <td class="name" title=${label}>${label}</td>
      <td><span class="kind-badge ${kindClass}">${st.kind}</span></td>
      <td>${st.count.toLocaleString()}</td>
      <td>${fmtNs(st.p50_ns)}</td>
      <td>${fmtNs(st.p95_ns)}</td>
      <td>${fmtNs(st.p99_ns)}</td>
      <td>${fmtNs(st.max_ns)}</td>
      <td>${quality != null ? `${(quality * 100).toFixed(0)}%` : "—"}</td>
    </tr>
  `;
}

/** Render the catalog body and reveal (or hide) the table. */
export function renderCatalog(
  els: { catalogBody: HTMLElement; catalogWrap: HTMLElement },
  spanTypes: readonly SpanTypeStats[],
  sort: CatalogSort,
  selectedUid: string | null,
  onSelect: (uid: string) => void,
): void {
  const sorted = sortedForDisplay(spanTypes, sort);
  render(
    html`${sorted.map((st) => rowTemplate(st, selectedUid, onSelect))}`,
    els.catalogBody,
  );
  els.catalogWrap.style.display = sorted.length > 0 ? "block" : "none";
}
