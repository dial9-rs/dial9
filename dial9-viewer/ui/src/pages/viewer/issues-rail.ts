// The issues rail: a ranked, keyboard-navigable, sortable list of points of
// interest. A store-wired controller (createIssuesRail) created once so its
// sort memo lives across renders; it exposes a lit-html `template(state)` and
// `n`/`p` key bindings. Every handler dispatches store actions only - it never
// renders; the shell's store subscription repaints.
//
// The rail reads the derived POI model (poi.ts). Row click and `n`/`p` center
// the viewport on the POI and select its task; the filter dropdown and
// column-header sort re-scope/re-order the SAME detector rows, so the count is
// unchanged.

import { html, type TemplateResult } from "lit-html";
import { classMap } from "lit-html/directives/class-map.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import type { PoiSortKey } from "../../types/state.js";
import type { PointOfInterestType } from "../../types/trace.js";
import type { KeyBinding } from "../../lib/interact/keyboard.js";
import {
  POI_FILTERS,
  derivePoiViewModel,
  filterLabel,
  parsePoiFilter,
  poiJump,
  stepIndex,
  type PoiViewModel,
} from "./poi.js";

/** A sortable column: its state key, header label, and default direction. */
interface Column {
  key: PoiSortKey;
  label: string;
  /** Column-scoped tooltip. */
  title: string;
  /** Direction applied on the FIRST click of this column. */
  defaultDir: "asc" | "desc";
}

const COLUMNS: readonly Column[] = [
  { key: "worker", label: "worker", title: "Sort by worker thread", defaultDir: "asc" },
  { key: "kind", label: "what", title: "Sort by issue kind", defaultDir: "asc" },
  { key: "time", label: "t", title: "Sort by time (chronological)", defaultDir: "asc" },
  { key: "duration", label: "dur", title: "Sort by severity (worst first)", defaultDir: "desc" },
];

export interface IssuesRailController {
  /** The rail template for one render pass (reads live store state). */
  template(state: StoreState): TemplateResult;
  /** `n`/`p` step bindings for the unified key router. */
  keyBindings: readonly KeyBinding[];
  /** No live resources; present for symmetry with the track controllers. */
  dispose(): void;
}

/** Build the store-wired issues-rail controller. */
export function createIssuesRail(store: ViewerStore): IssuesRailController {
  // Sort memo: derivePoiViewModel re-sorts the whole list, so cache it on the
  // (trace, filter, sortKey, sortDir, minTs) key and skip re-sorting on a
  // pan/zoom/selection repaint (the common frame). The `index` is applied
  // fresh from live state, so stepping never busts the sort cache.
  let cacheKey = "";
  let cacheVm: PoiViewModel | null = null;

  function viewModel(state: StoreState): PoiViewModel {
    const trace = state.trace.trace;
    const { poi, viewport } = state;
    const key = trace
      ? `${idOf(trace)}|${poi.filter}|${poi.sortKey}|${poi.sortDir}|${viewport.minTs}`
      : "none";
    if (cacheVm === null || key !== cacheKey) {
      cacheKey = key;
      cacheVm = derivePoiViewModel(trace, poi, viewport.minTs);
    }
    // Re-clamp the live index onto the cached (index-independent) rows.
    const index = poi.index < cacheVm.total ? poi.index : -1;
    return { ...cacheVm, index };
  }

  /** Center + select on the POI at `index` of the current sorted list. */
  function jumpTo(index: number): void {
    const vm = viewModel(store.getState());
    const row = vm.rows[index];
    if (row === undefined) return;
    const jump = poiJump(row.poi, store.getState().viewport);
    store.update("viewport", { viewStart: jump.viewStart, viewEnd: jump.viewEnd });
    store.update("selection", { selectedTaskId: jump.selectedTaskId });
    store.update("poi", { index });
  }

  /** `n`/`p`: step the current index and jump. */
  function step(dir: 1 | -1): boolean {
    const state = store.getState();
    const vm = viewModel(state);
    const next = stepIndex(vm.total, state.poi.index, dir);
    if (next < 0) return false; // nothing to step to - decline the key
    jumpTo(next);
    return true;
  }

  function setFilter(filter: PointOfInterestType): void {
    // A new filter rebuilds the list; the current index no longer maps.
    store.update("poi", { filter, index: -1 });
  }

  /** A column header click: toggle direction if already active, else its
   *  default direction. Index is preserved by re-clamping (a re-sort keeps
   *  the same rows), matching the "sort is count-independent" contract. */
  function sortByColumn(col: Column): void {
    const { poi } = store.getState();
    const dir =
      poi.sortKey === col.key
        ? poi.sortDir === "asc"
          ? "desc"
          : "asc"
        : col.defaultDir;
    store.update("poi", { sortKey: col.key, sortDir: dir, index: -1 });
  }

  const keyBindings: readonly KeyBinding[] = [
    { key: "n", onKey: () => step(1) },
    { key: "p", onKey: () => step(-1) },
  ];

  return {
    template: (state) => railTemplate(viewModel(state), { setFilter, sortByColumn, jumpTo }),
    keyBindings,
    dispose(): void {
      cacheVm = null;
    },
  };
}

interface RailHandlers {
  setFilter(filter: PointOfInterestType): void;
  sortByColumn(col: Column): void;
  jumpTo(index: number): void;
}

/** The rail's lit-html template for one render pass. */
function railTemplate(vm: PoiViewModel, h: RailHandlers): TemplateResult {
  const positionLabel =
    vm.total === 0
      ? "None found"
      : `${vm.index >= 0 ? vm.index + 1 : 0}/${vm.total}`;

  return html`
    <aside class="d9-rail" role="region" aria-label="Issues">
      <div class="d9-rail-head">
        <div class="d9-rail-title">
          <span class="d9-rail-heading">ISSUES</span>
          <span class="d9-rail-pos" data-poi-position>${positionLabel}</span>
        </div>
        <div class="d9-rail-controls">
          <label class="d9-rail-filter-label">
            <span class="d9-sr-only">Point-of-interest filter</span>
            <select
              class="d9-rail-filter"
              data-poi-filter
              aria-label="Point-of-interest filter"
              title="Which class of issue to list"
              @change=${(e: Event) => {
                const filter = parsePoiFilter((e.target as HTMLSelectElement).value);
                if (filter !== null) h.setFilter(filter);
              }}
            >
              ${POI_FILTERS.map(
                (f) => html`<option value=${f} ?selected=${f === vm.filter}>
                  ${filterLabel(f)}
                </option>`,
              )}
            </select>
          </label>
          <span class="d9-rail-hint" title="Step issues with the n / p keys"
            ><kbd>n</kbd>/<kbd>p</kbd> step</span
          >
        </div>
      </div>

      ${vm.total === 0
        ? html`<p class="d9-rail-empty">No issues match this filter.</p>`
        : railTable(vm, h)}
    </aside>
  `;
}

function railTable(vm: PoiViewModel, h: RailHandlers): TemplateResult {
  return html`
    <div class="d9-rail-list">
      <table
        class="d9-rail-table"
        role="listbox"
        aria-label="Points of interest"
        aria-activedescendant=${vm.index >= 0 ? `d9-poi-${vm.index}` : ""}
      >
        <thead>
          <tr>
            <th scope="col" aria-hidden="true"></th>
            ${COLUMNS.map(
              (col) => html`<th scope="col">
                <button
                  type="button"
                  class=${classMap({
                    "d9-rail-sort": true,
                    on: vm.sortKey === col.key,
                  })}
                  title=${col.title}
                  aria-label=${col.title}
                  @click=${() => h.sortByColumn(col)}
                >
                  ${col.label}${vm.sortKey === col.key
                    ? html`<span aria-hidden="true"
                        >${vm.sortDir === "asc" ? " ^" : " v"}</span
                      >`
                    : ""}
                </button>
              </th>`,
            )}
          </tr>
        </thead>
        <tbody>
          ${vm.rows.map(
            (row, i) => html`
              <tr
                id=${`d9-poi-${i}`}
                class=${classMap({ "d9-rail-row": true, on: i === vm.index })}
                role="option"
                aria-selected=${i === vm.index ? "true" : "false"}
                title=${`${row.worker} · ${row.kind} · ${row.time} · ${row.duration}`}
                @click=${() => h.jumpTo(i)}
              >
                <td class="d9-rail-dot-cell">
                  <span
                    class=${classMap({
                      "d9-rail-dot": true,
                      [`sev-${row.severity}`]: true,
                    })}
                    aria-hidden="true"
                  ></span>
                </td>
                <td class="d9-rail-worker">${row.worker}</td>
                <td>${row.kind}</td>
                <td class="d9-rail-num">${row.time}</td>
                <td class="d9-rail-num">${row.duration}</td>
              </tr>
            `,
          )}
        </tbody>
      </table>
    </div>
  `;
}

/** A stable per-trace identity string for the sort-memo key. */
let idSeq = 0;
const idMap = new WeakMap<object, number>();
function idOf(trace: object): number {
  let id = idMap.get(trace);
  if (id === undefined) {
    id = ++idSeq;
    idMap.set(trace, id);
  }
  return id;
}
